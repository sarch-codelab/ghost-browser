//! DOM nativo en Rust puro conectado a `wisp_js::vm` (sin Boa ni V8).
//!
//! Provee un arena mutable sobre `scraper::Html`, bindings para `document`,
//! `window`, `Element`, `style`, `classList`, eventos, `setTimeout`,
//! `requestAnimationFrame`, `Promise`, `URL`, `TextEncoder`/`TextDecoder`,
//! `localStorage`, `location`, módulos ES básicos y `fetch` con política SSRF.

#![forbid(unsafe_code)]

use crate::vm::{HostBridge, JsVal, VmRuntime};
use crate::{
    ElementGeometry, EvalResult, FETCH_STATE, GeometryHook, MAX_FETCH_TOTAL_TIME,
    MAX_SCRIPT_WARNINGS, MAX_TREE_DEPTH, ModuleEntry, fetch_json, push_warning,
    record_fetch_warning, resolve_script_url, truncate_error,
};
use ego_tree::NodeRef;
use scraper::Html;
use scraper::node::Node;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

const MAX_DOM_NODES: usize = 32_768;
const MAX_SERIALIZED_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_TASK_DRAIN_ITERS: usize = 256;

#[derive(Clone, Debug)]
enum NodeKind {
    Document,
    Element {
        tag: String,
        attrs: HashMap<String, String>,
        styles: Vec<(String, String)>,
    },
    Text(String),
}

#[derive(Clone, Debug)]
struct ArenaNode {
    kind: NodeKind,
    parent: Option<usize>,
    children: Vec<usize>,
}

#[derive(Clone, Debug)]
struct PromiseWaiter {
    cb_fn_id: Option<usize>,
    target_promise: usize,
    is_catch: bool,
}

#[derive(Clone, Debug)]
struct PromiseState {
    resolved: Option<JsVal>,
    rejected: Option<JsVal>,
    waiters: Vec<PromiseWaiter>,
}

struct Microtask {
    cb_fn_id: usize,
    arg: JsVal,
    target_promise: usize,
}

#[derive(Clone, Debug)]
struct MutationRecord {
    kind: String,
    target: usize,
    attribute_name: Option<String>,
}

#[derive(Clone, Debug)]
struct MutationObserverState {
    cb_fn_id: usize,
    targets: Vec<usize>,
    subtree: bool,
    attributes: bool,
    child_list: bool,
    records: Vec<MutationRecord>,
}

pub struct NativeDomHost<'a> {
    nodes: Vec<ArenaNode>,
    doc_id: usize,
    html_id: Option<usize>,
    head_id: Option<usize>,
    body_id: Option<usize>,
    mutated: bool,
    oversized: bool,
    layout_dirty: bool,
    viewport_width: f32,
    geometry_hook: Option<GeometryHook<'a>>,
    geometry_cache: HashMap<usize, ElementGeometry>,
    page_url: Option<String>,
    policy: wisp_net::NetworkPolicy,
    cookie_jar: wisp_net::CookieJar,
    storage: HashMap<String, String>,
    warnings: Vec<String>,
    doc_listeners: HashMap<String, Vec<usize>>,
    win_listeners: HashMap<String, Vec<usize>>,
    node_listeners: HashMap<(usize, String), Vec<usize>>,
    promises: Vec<PromiseState>,
    microtasks: Vec<Microtask>,
    timers: Vec<usize>,
    observers: Vec<MutationObserverState>,
    started_at: Instant,
    scroll_x: f64,
    scroll_y: f64,
}

impl<'a> NativeDomHost<'a> {
    fn from_html(
        document: &Html,
        page_url: Option<&str>,
        viewport_width: f32,
        policy: wisp_net::NetworkPolicy,
        geometry_hook: Option<GeometryHook<'a>>,
    ) -> Self {
        let mut host = Self {
            nodes: vec![ArenaNode {
                kind: NodeKind::Document,
                parent: None,
                children: Vec::new(),
            }],
            doc_id: 0,
            html_id: None,
            head_id: None,
            body_id: None,
            mutated: false,
            oversized: false,
            layout_dirty: true,
            viewport_width: if viewport_width > 0.0 {
                viewport_width
            } else {
                1280.0
            },
            geometry_hook,
            geometry_cache: HashMap::new(),
            page_url: page_url.map(str::to_owned),
            policy,
            cookie_jar: wisp_net::CookieJar::new(),
            storage: HashMap::new(),
            warnings: Vec::new(),
            doc_listeners: HashMap::new(),
            win_listeners: HashMap::new(),
            node_listeners: HashMap::new(),
            promises: Vec::new(),
            microtasks: Vec::new(),
            timers: Vec::new(),
            observers: Vec::new(),
            started_at: Instant::now(),
            scroll_x: 0.0,
            scroll_y: 0.0,
        };
        for child in document.tree.root().children() {
            let _ = host.import_node(child, 0, 0);
        }
        host.refresh_landmarks();
        host
    }

    fn mark_mutated(&mut self, target: usize, kind: &str, attr_name: Option<&str>) {
        self.mutated = true;
        self.layout_dirty = true;
        for obs in &mut self.observers {
            let type_ok = match kind {
                "attributes" => obs.attributes,
                _ => obs.child_list,
            };
            if !type_ok {
                continue;
            }
            let target_ok = obs.targets.iter().any(|&observed| {
                if observed == target {
                    return true;
                }
                if !obs.subtree {
                    return false;
                }
                let mut cur = self.nodes.get(target).and_then(|n| n.parent);
                while let Some(p) = cur {
                    if p == observed {
                        return true;
                    }
                    cur = self.nodes.get(p).and_then(|n| n.parent);
                }
                false
            });
            if target_ok {
                obs.records.push(MutationRecord {
                    kind: kind.to_owned(),
                    target,
                    attribute_name: attr_name.map(str::to_owned),
                });
            }
        }
    }

    fn ensure_geometry(&mut self) {
        if !self.layout_dirty {
            return;
        }
        self.geometry_cache.clear();
        let mut preorder_map = Vec::new();
        self.collect_preorder_elements(self.doc_id, &mut preorder_map);
        if let Some(hook) = self.geometry_hook.as_deref_mut() {
            let html = self.serialize_html();
            let by_preorder = hook(&html, self.viewport_width);
            for (pre_idx, nid) in preorder_map.into_iter().enumerate() {
                if let Some(geom) = by_preorder.get(&pre_idx) {
                    self.geometry_cache.insert(nid, geom.clone());
                }
            }
        } else {
            self.compute_fallback_geometry();
        }
        self.layout_dirty = false;
    }

    fn collect_preorder_elements(&self, nid: usize, out: &mut Vec<usize>) {
        let Some(node) = self.nodes.get(nid) else {
            return;
        };
        if matches!(node.kind, NodeKind::Element { .. }) {
            out.push(nid);
        }
        for &c in &node.children {
            self.collect_preorder_elements(c, out);
        }
    }

    fn compute_fallback_geometry(&mut self) {
        let vw = self.viewport_width;
        let mut y = 0.0f32;
        self.layout_fallback_node(self.doc_id, 0.0, &mut y, vw);
    }

    fn layout_fallback_node(&mut self, nid: usize, x: f32, y: &mut f32, avail_w: f32) {
        let Some(node) = self.nodes.get(nid).cloned() else {
            return;
        };
        match &node.kind {
            NodeKind::Document => {
                for &c in &node.children {
                    self.layout_fallback_node(c, x, y, avail_w);
                }
            }
            NodeKind::Text(_) => {}
            NodeKind::Element { tag, styles, .. } => {
                let mut display = if matches!(
                    tag.as_str(),
                    "span" | "a" | "strong" | "em" | "b" | "i" | "label"
                ) {
                    "inline".to_owned()
                } else if matches!(
                    tag.as_str(),
                    "head" | "script" | "style" | "meta" | "title" | "link"
                ) {
                    "none".to_owned()
                } else {
                    "block".to_owned()
                };
                let mut visibility = "visible".to_owned();
                let mut position = "static".to_owned();
                let mut color = "rgb(0, 0, 0)".to_owned();
                let mut background_color = "rgba(0, 0, 0, 0)".to_owned();
                let mut font_size = "16px".to_owned();
                let mut explicit_w: Option<f32> = None;
                let mut explicit_h: Option<f32> = None;

                for (k, v) in styles {
                    match k.as_str() {
                        "display" => display = v.to_ascii_lowercase(),
                        "visibility" => visibility = v.to_ascii_lowercase(),
                        "position" => position = v.to_ascii_lowercase(),
                        "color" => color = v.clone(),
                        "background-color" | "background" => background_color = v.clone(),
                        "font-size" => font_size = v.clone(),
                        "width" => explicit_w = parse_px_val(v),
                        "height" => explicit_h = parse_px_val(v),
                        _ => {}
                    }
                }

                if display == "none" {
                    self.geometry_cache.insert(
                        nid,
                        ElementGeometry {
                            x: 0.0,
                            y: 0.0,
                            width: 0.0,
                            height: 0.0,
                            display,
                            visibility,
                            position,
                            color,
                            background_color,
                            font_size,
                        },
                    );
                    return;
                }

                let start_y = *y;
                let width = explicit_w.unwrap_or(avail_w);
                let mut child_y = start_y;
                for &c in &node.children {
                    self.layout_fallback_node(c, x, &mut child_y, width);
                }
                let mut text = String::new();
                self.collect_text(nid, &mut text);
                let intrinsic_h = if !text.trim().is_empty() && child_y == start_y {
                    20.0
                } else {
                    (child_y - start_y).max(0.0)
                };
                let height = explicit_h.unwrap_or(intrinsic_h);
                *y = start_y + height;

                self.geometry_cache.insert(
                    nid,
                    ElementGeometry {
                        x,
                        y: start_y,
                        width,
                        height,
                        display,
                        visibility,
                        position,
                        color,
                        background_color,
                        font_size,
                    },
                );
            }
        }
    }

    fn refresh_landmarks(&mut self) {
        self.html_id = self.find_first_tag(self.doc_id, "html");
        self.head_id = self.find_first_tag(self.doc_id, "head");
        self.body_id = self.find_first_tag(self.doc_id, "body");
    }

    fn find_first_tag(&self, root: usize, tag: &str) -> Option<usize> {
        let node = self.nodes.get(root)?;
        if let NodeKind::Element { tag: t, .. } = &node.kind
            && t.eq_ignore_ascii_case(tag)
        {
            return Some(root);
        }
        for &child in &node.children {
            if let Some(found) = self.find_first_tag(child, tag) {
                return Some(found);
            }
        }
        None
    }

    fn import_node(
        &mut self,
        node: NodeRef<'_, Node>,
        parent: usize,
        depth: usize,
    ) -> Option<usize> {
        if depth > MAX_TREE_DEPTH || self.nodes.len() >= MAX_DOM_NODES {
            self.oversized = true;
            return None;
        }
        match node.value() {
            Node::Text(text) => {
                let id = self.alloc_node(NodeKind::Text(text.text.to_string()));
                self.attach_child(parent, id);
                Some(id)
            }
            Node::Element(el) => {
                let mut attrs = HashMap::new();
                let mut styles = Vec::new();
                for (k, v) in el.attrs() {
                    if k.eq_ignore_ascii_case("style") {
                        styles = parse_inline_styles(v);
                    } else {
                        attrs.insert(k.to_owned(), v.to_owned());
                    }
                }
                let id = self.alloc_node(NodeKind::Element {
                    tag: el.name().to_owned(),
                    attrs,
                    styles,
                });
                self.attach_child(parent, id);
                for child in node.children() {
                    let _ = self.import_node(child, id, depth + 1);
                }
                Some(id)
            }
            Node::Document | Node::Fragment => {
                for child in node.children() {
                    let _ = self.import_node(child, parent, depth + 1);
                }
                None
            }
            Node::Doctype(_) | Node::Comment(_) | Node::ProcessingInstruction(_) => None,
        }
    }

    fn alloc_node(&mut self, kind: NodeKind) -> usize {
        let id = self.nodes.len();
        self.nodes.push(ArenaNode {
            kind,
            parent: None,
            children: Vec::new(),
        });
        id
    }

    fn detach_node(&mut self, id: usize) {
        if let Some(old_parent) = self.nodes.get(id).and_then(|n| n.parent)
            && let Some(parent) = self.nodes.get_mut(old_parent)
        {
            parent.children.retain(|&c| c != id);
        }
        if let Some(node) = self.nodes.get_mut(id) {
            node.parent = None;
        }
    }

    fn attach_child(&mut self, parent: usize, child: usize) {
        if parent == child {
            return;
        }
        self.detach_node(child);
        if let Some(p) = self.nodes.get_mut(parent) {
            p.children.push(child);
        }
        if let Some(c) = self.nodes.get_mut(child) {
            c.parent = Some(parent);
        }
    }

    fn prepend_child(&mut self, parent: usize, child: usize) {
        if parent == child {
            return;
        }
        self.detach_node(child);
        if let Some(p) = self.nodes.get_mut(parent) {
            p.children.insert(0, child);
        }
        if let Some(c) = self.nodes.get_mut(child) {
            c.parent = Some(parent);
        }
    }

    fn init_globals(&self, vm: &mut VmRuntime) {
        vm.globals.insert(
            "document".to_owned(),
            JsVal::NativeHandle("document".to_owned()),
        );
        vm.globals.insert(
            "window".to_owned(),
            JsVal::NativeHandle("window".to_owned()),
        );
        vm.globals.insert(
            "location".to_owned(),
            JsVal::NativeHandle("location".to_owned()),
        );
        vm.globals.insert(
            "localStorage".to_owned(),
            JsVal::NativeHandle("localStorage".to_owned()),
        );
        vm.globals.insert(
            "console".to_owned(),
            JsVal::NativeHandle("console".to_owned()),
        );
        vm.globals
            .insert("JSON".to_owned(), JsVal::NativeHandle("JSON".to_owned()));
        vm.globals.insert(
            "Promise".to_owned(),
            JsVal::NativeHandle("Promise".to_owned()),
        );
        vm.globals
            .insert("Array".to_owned(), JsVal::NativeHandle("Array".to_owned()));
        vm.globals.insert(
            "navigator".to_owned(),
            JsVal::NativeHandle("navigator".to_owned()),
        );
        vm.globals.insert(
            "performance".to_owned(),
            JsVal::NativeHandle("performance".to_owned()),
        );
        vm.globals.insert(
            "crypto".to_owned(),
            JsVal::NativeHandle("crypto".to_owned()),
        );
        if self.policy.stealth {
            let runtime = vm.alloc_object(HashMap::new());
            let chrome = vm.alloc_object(HashMap::from([("runtime".to_owned(), runtime)]));
            vm.globals.insert("chrome".to_owned(), chrome);
        }
    }

    fn collect_text(&self, id: usize, out: &mut String) {
        let Some(node) = self.nodes.get(id) else {
            return;
        };
        match &node.kind {
            NodeKind::Text(t) => out.push_str(t),
            NodeKind::Document | NodeKind::Element { .. } => {
                for &c in &node.children {
                    self.collect_text(c, out);
                }
            }
        }
    }

    fn set_text_content(&mut self, id: usize, text: String) {
        if let Some(node) = self.nodes.get_mut(id) {
            match &mut node.kind {
                NodeKind::Text(t) => *t = text,
                NodeKind::Element { .. } => {
                    let old_children = std::mem::take(&mut node.children);
                    for c in old_children {
                        if let Some(cn) = self.nodes.get_mut(c) {
                            cn.parent = None;
                        }
                    }
                    if !text.is_empty() {
                        let txt_id = self.alloc_node(NodeKind::Text(text));
                        self.attach_child(id, txt_id);
                    }
                }
                NodeKind::Document => {}
            }
            self.mutated = true;
        }
    }

    fn set_inner_html(&mut self, id: usize, raw: &str) {
        if let Some(node) = self.nodes.get_mut(id) {
            let old = std::mem::take(&mut node.children);
            for c in old {
                if let Some(cn) = self.nodes.get_mut(c) {
                    cn.parent = None;
                }
            }
        }
        let frag = Html::parse_fragment(raw);
        for child in frag.tree.root().children() {
            let _ = self.import_node(child, id, 0);
        }
        self.mutated = true;
    }

    fn insert_adjacent_html(&mut self, id: usize, pos: &str, raw: &str) {
        let frag = Html::parse_fragment(raw);
        let mut inserted = Vec::new();
        for child in frag.tree.root().children() {
            if let Some(cid) = self.import_node_detached(child, 0) {
                inserted.push(cid);
            }
        }
        match pos.to_ascii_lowercase().as_str() {
            "afterbegin" => {
                for cid in inserted.into_iter().rev() {
                    self.prepend_child(id, cid);
                }
            }
            "beforeend" => {
                for cid in inserted {
                    self.attach_child(id, cid);
                }
            }
            _ => {
                for cid in inserted {
                    self.attach_child(id, cid);
                }
            }
        }
        self.mutated = true;
    }

    fn import_node_detached(&mut self, node: NodeRef<'_, Node>, depth: usize) -> Option<usize> {
        if depth > MAX_TREE_DEPTH || self.nodes.len() >= MAX_DOM_NODES {
            self.oversized = true;
            return None;
        }
        match node.value() {
            Node::Text(text) => Some(self.alloc_node(NodeKind::Text(text.text.to_string()))),
            Node::Element(el) => {
                let mut attrs = HashMap::new();
                let mut styles = Vec::new();
                for (k, v) in el.attrs() {
                    if k.eq_ignore_ascii_case("style") {
                        styles = parse_inline_styles(v);
                    } else {
                        attrs.insert(k.to_owned(), v.to_owned());
                    }
                }
                let id = self.alloc_node(NodeKind::Element {
                    tag: el.name().to_owned(),
                    attrs,
                    styles,
                });
                for child in node.children() {
                    let _ = self.import_node(child, id, depth + 1);
                }
                Some(id)
            }
            Node::Document | Node::Fragment => {
                for child in node.children() {
                    if let Some(cid) = self.import_node_detached(child, depth + 1) {
                        return Some(cid);
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn get_element_by_id(&self, id_str: &str) -> Option<usize> {
        if id_str.is_empty() {
            return None;
        }
        self.walk_elements(self.doc_id, &|node| {
            matches!(
                &node.kind,
                NodeKind::Element { attrs, .. } if attrs.get("id").is_some_and(|v| v == id_str)
            )
        })
        .into_iter()
        .next()
    }

    fn get_elements_by_class_name(&self, cls: &str) -> Vec<usize> {
        let wanted: Vec<&str> = cls.split_whitespace().collect();
        if wanted.is_empty() {
            return Vec::new();
        }
        self.walk_elements(self.doc_id, &|node| {
            let NodeKind::Element { attrs, .. } = &node.kind else {
                return false;
            };
            let class_attr = attrs.get("class").map(String::as_str).unwrap_or("");
            wanted
                .iter()
                .all(|w| class_attr.split_whitespace().any(|c| c == *w))
        })
    }

    fn get_elements_by_tag_name(&self, tag: &str) -> Vec<usize> {
        if tag.is_empty() {
            return Vec::new();
        }
        let wildcard = tag == "*";
        self.walk_elements(self.doc_id, &|node| {
            matches!(
                &node.kind,
                NodeKind::Element { tag: t, .. } if wildcard || t.eq_ignore_ascii_case(tag)
            )
        })
    }

    fn query_selector_all(&self, root: usize, selector: &str) -> Vec<usize> {
        let mut results = Vec::new();
        let all = self.walk_elements(root, &|_| true);
        let branches: Vec<&str> = selector.split(',').map(str::trim).collect();
        for nid in all {
            if nid == self.doc_id {
                continue;
            }
            if branches
                .iter()
                .any(|branch| !branch.is_empty() && self.matches_complex_selector(nid, branch))
            {
                results.push(nid);
            }
        }
        results
    }

    fn matches_complex_selector(&self, nid: usize, selector: &str) -> bool {
        let tokens = parse_selector_chain(selector);
        if tokens.is_empty() {
            return false;
        }
        let Some((last_comb, last_comp)) = tokens.last() else {
            return false;
        };
        let _ = last_comb;
        if !self.matches_compound(nid, last_comp) {
            return false;
        }
        let mut cur = nid;
        for idx in (0..tokens.len() - 1).rev() {
            let (comb, comp) = &tokens[idx + 1];
            let target_comp = &tokens[idx].1;
            let _ = comp;
            match comb {
                Combinator::Child => {
                    let Some(parent) = self.parent_element(cur) else {
                        return false;
                    };
                    if !self.matches_compound(parent, target_comp) {
                        return false;
                    }
                    cur = parent;
                }
                Combinator::Descendant => {
                    let mut anc = self.parent_element(cur);
                    let mut matched = None;
                    while let Some(a) = anc {
                        if self.matches_compound(a, target_comp) {
                            matched = Some(a);
                            break;
                        }
                        anc = self.parent_element(a);
                    }
                    let Some(found) = matched else {
                        return false;
                    };
                    cur = found;
                }
            }
        }
        true
    }

    fn parent_element(&self, nid: usize) -> Option<usize> {
        let p = self.nodes.get(nid)?.parent?;
        match self.nodes.get(p)?.kind {
            NodeKind::Element { .. } => Some(p),
            _ => None,
        }
    }

    fn matches_compound(&self, nid: usize, comp: &str) -> bool {
        let Some(node) = self.nodes.get(nid) else {
            return false;
        };
        let NodeKind::Element { tag, attrs, .. } = &node.kind else {
            return false;
        };
        let mut rest = comp.trim();
        if rest.is_empty() {
            return false;
        }
        let mut first_child_req = false;
        if let Some(stripped) = rest.strip_suffix(":first-child") {
            first_child_req = true;
            rest = stripped;
        }
        if first_child_req {
            let Some(parent_id) = node.parent else {
                return false;
            };
            let first_el = self.nodes[parent_id]
                .children
                .iter()
                .copied()
                .find(|&c| matches!(self.nodes[c].kind, NodeKind::Element { .. }));
            if first_el != Some(nid) {
                return false;
            }
        }
        if rest.is_empty() {
            return true;
        }
        // Parse tag, #id, .class segments
        let mut tag_part: Option<&str> = None;
        let mut i = 0usize;
        let bytes = rest.as_bytes();
        while i < bytes.len() && bytes[i] != b'#' && bytes[i] != b'.' {
            i += 1;
        }
        if i > 0 {
            tag_part = Some(&rest[..i]);
        }
        if let Some(tp) = tag_part
            && tp != "*"
            && !tag.eq_ignore_ascii_case(tp)
        {
            return false;
        }
        while i < bytes.len() {
            let kind = bytes[i];
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != b'#' && bytes[i] != b'.' {
                i += 1;
            }
            let ident = &rest[start..i];
            if ident.is_empty() {
                return false;
            }
            if kind == b'#' {
                if attrs.get("id").is_none_or(|v| v != ident) {
                    return false;
                }
            } else if kind == b'.' {
                let classes = attrs.get("class").map(String::as_str).unwrap_or("");
                if !classes.split_whitespace().any(|c| c == ident) {
                    return false;
                }
            }
        }
        true
    }

    fn walk_elements(&self, root: usize, pred: &dyn Fn(&ArenaNode) -> bool) -> Vec<usize> {
        let mut out = Vec::new();
        self.walk_rec(root, pred, &mut out);
        out
    }

    fn walk_rec(&self, id: usize, pred: &dyn Fn(&ArenaNode) -> bool, out: &mut Vec<usize>) {
        let Some(node) = self.nodes.get(id) else {
            return;
        };
        if matches!(node.kind, NodeKind::Element { .. }) && pred(node) {
            out.push(id);
        }
        for &c in &node.children {
            self.walk_rec(c, pred, out);
        }
    }

    fn serialize_html(&self) -> String {
        let mut out = String::new();
        for &c in &self.nodes[self.doc_id].children {
            self.serialize_node(c, &mut out);
        }
        out
    }

    fn serialize_node(&self, id: usize, out: &mut String) {
        let Some(node) = self.nodes.get(id) else {
            return;
        };
        match &node.kind {
            NodeKind::Document => {
                for &c in &node.children {
                    self.serialize_node(c, out);
                }
            }
            NodeKind::Text(t) => {
                out.push_str(&escape_text(t));
            }
            NodeKind::Element { tag, attrs, styles } => {
                out.push('<');
                out.push_str(tag);
                let mut keys: Vec<&String> = attrs.keys().collect();
                keys.sort();
                for k in keys {
                    if let Some(v) = attrs.get(k) {
                        out.push(' ');
                        out.push_str(k);
                        out.push_str("=\"");
                        out.push_str(&escape_attr(v));
                        out.push('"');
                    }
                }
                if !styles.is_empty() {
                    let style_str: Vec<String> =
                        styles.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                    out.push_str(" style=\"");
                    out.push_str(&escape_attr(&style_str.join("; ")));
                    out.push('"');
                }
                out.push('>');
                if is_void_tag(tag) {
                    return;
                }
                for &c in &node.children {
                    self.serialize_node(c, out);
                }
                out.push_str("</");
                out.push_str(tag);
                out.push('>');
            }
        }
    }

    fn alloc_promise(&mut self, resolved: Option<JsVal>, rejected: Option<JsVal>) -> JsVal {
        let id = self.promises.len();
        self.promises.push(PromiseState {
            resolved,
            rejected,
            waiters: Vec::new(),
        });
        JsVal::NativeHandle(format!("promise:{id}"))
    }

    fn settle_promise(&mut self, pid: usize, ok: bool, val: JsVal) {
        if ok
            && let JsVal::NativeHandle(nh) = &val
            && let Some(rest) = nh.strip_prefix("promise:")
            && let Ok(inner_id) = rest.parse::<usize>()
        {
            let inner_state = self
                .promises
                .get(inner_id)
                .map(|p| (p.resolved.clone(), p.rejected.clone()));
            match inner_state {
                Some((Some(v), _)) => {
                    self.settle_promise(pid, true, v);
                    return;
                }
                Some((_, Some(e))) => {
                    self.settle_promise(pid, false, e);
                    return;
                }
                Some((None, None)) => {
                    if let Some(inner) = self.promises.get_mut(inner_id) {
                        inner.waiters.push(PromiseWaiter {
                            cb_fn_id: None,
                            target_promise: pid,
                            is_catch: false,
                        });
                    }
                    return;
                }
                None => {}
            }
        }
        let waiters = {
            let Some(p) = self.promises.get_mut(pid) else {
                return;
            };
            if p.resolved.is_some() || p.rejected.is_some() {
                return;
            }
            if ok {
                p.resolved = Some(val.clone());
            } else {
                p.rejected = Some(val.clone());
            }
            std::mem::take(&mut p.waiters)
        };
        for w in waiters {
            match w.cb_fn_id {
                None => self.settle_promise(w.target_promise, ok, val.clone()),
                Some(fn_id) if (ok && !w.is_catch) || (!ok && w.is_catch) => {
                    self.microtasks.push(Microtask {
                        cb_fn_id: fn_id,
                        arg: val.clone(),
                        target_promise: w.target_promise,
                    });
                }
                Some(_) => self.settle_promise(w.target_promise, ok, val.clone()),
            }
        }
    }

    fn flush_mutation_observers(&mut self, vm: &mut VmRuntime) -> bool {
        let mut flushed = false;
        let mut pending = Vec::new();
        for (idx, obs) in self.observers.iter_mut().enumerate() {
            if !obs.records.is_empty() {
                pending.push((idx, obs.cb_fn_id, std::mem::take(&mut obs.records)));
            }
        }
        for (_idx, cb_fn_id, recs) in pending {
            flushed = true;
            let js_recs: Vec<JsVal> = recs
                .into_iter()
                .map(|r| {
                    vm.alloc_object(HashMap::from([
                        ("type".to_owned(), JsVal::Str(r.kind)),
                        ("target".to_owned(), JsVal::DomNode(r.target)),
                        (
                            "attributeName".to_owned(),
                            r.attribute_name.map(JsVal::Str).unwrap_or(JsVal::Null),
                        ),
                    ]))
                })
                .collect();
            let _ = vm.invoke_fn(cb_fn_id, vec![JsVal::Array(js_recs)], self);
        }
        flushed
    }

    fn drain_async_tasks(&mut self, vm: &mut VmRuntime) {
        let mut iters = 0usize;
        while iters < MAX_TASK_DRAIN_ITERS {
            iters += 1;
            if self.flush_mutation_observers(vm) {
                continue;
            }
            if !self.microtasks.is_empty() {
                let batch = std::mem::take(&mut self.microtasks);
                for task in batch {
                    match vm.invoke_fn(task.cb_fn_id, vec![task.arg], self) {
                        Ok(val) => self.settle_promise(task.target_promise, true, val),
                        Err(err) => {
                            self.settle_promise(task.target_promise, false, JsVal::Str(err))
                        }
                    }
                }
                continue;
            }
            if !self.timers.is_empty() {
                let timers = std::mem::take(&mut self.timers);
                for fn_id in timers {
                    if let Err(err) = vm.invoke_fn(fn_id, Vec::new(), self) {
                        push_warning(&mut self.warnings, format!("Falló timer JS: {err}"));
                    }
                }
                continue;
            }
            break;
        }
    }

    fn dispatch_lifecycle_events(&mut self, vm: &mut VmRuntime) {
        if let Some(fns) = self.doc_listeners.get("DOMContentLoaded").cloned() {
            let ev = vm.alloc_object(HashMap::from([(
                "type".to_owned(),
                JsVal::Str("DOMContentLoaded".to_owned()),
            )]));
            for fn_id in fns {
                let _ = vm.invoke_fn(fn_id, vec![ev.clone()], self);
            }
        }
        self.drain_async_tasks(vm);
        if let Some(fns) = self.win_listeners.get("load").cloned() {
            let ev = vm.alloc_object(HashMap::from([(
                "type".to_owned(),
                JsVal::Str("load".to_owned()),
            )]));
            for fn_id in fns {
                let _ = vm.invoke_fn(fn_id, vec![ev.clone()], self);
            }
        }
        self.drain_async_tasks(vm);
    }
}

impl HostBridge for NativeDomHost<'_> {
    fn get_prop(&mut self, vm: &mut VmRuntime, target: &JsVal, prop: &str) -> Result<JsVal, JsVal> {
        match target {
            JsVal::NativeHandle(handle) => match handle.as_str() {
                "document" => match prop {
                    "body" => Ok(self.body_id.map(JsVal::DomNode).unwrap_or(JsVal::Null)),
                    "head" => Ok(self.head_id.map(JsVal::DomNode).unwrap_or(JsVal::Null)),
                    "documentElement" => {
                        Ok(self.html_id.map(JsVal::DomNode).unwrap_or(JsVal::Null))
                    }
                    "doctype" => Ok(JsVal::Null),
                    "readyState" => Ok(JsVal::Str("complete".to_owned())),
                    "URL" => Ok(JsVal::Str(self.page_url.clone().unwrap_or_default())),
                    "cookie" => {
                        let parsed = self
                            .page_url
                            .as_deref()
                            .and_then(|u| url::Url::parse(u).ok());
                        let (host, path, is_https) = match parsed {
                            Some(ref u) => (
                                u.host_str().unwrap_or("localhost").to_owned(),
                                u.path().to_owned(),
                                u.scheme() == "https",
                            ),
                            None => ("localhost".to_owned(), "/".to_owned(), false),
                        };
                        Ok(JsVal::Str(
                            self.cookie_jar
                                .cookie_header_for(&host, &path, is_https, true)
                                .unwrap_or_default(),
                        ))
                    }
                    "images" => Ok(JsVal::Array(
                        self.get_elements_by_tag_name("img")
                            .into_iter()
                            .map(JsVal::DomNode)
                            .collect(),
                    )),
                    "links" => Ok(JsVal::Array(
                        self.get_elements_by_tag_name("a")
                            .into_iter()
                            .map(JsVal::DomNode)
                            .collect(),
                    )),
                    "title" => {
                        let mut text = String::new();
                        if let Some(tid) = self.find_first_tag(self.doc_id, "title") {
                            self.collect_text(tid, &mut text);
                        }
                        Ok(JsVal::Str(text))
                    }
                    "location" => Ok(JsVal::NativeHandle("location".to_owned())),
                    _ => Ok(JsVal::Undefined),
                },
                "window" => match prop {
                    "window" | "self" => Ok(JsVal::NativeHandle("window".to_owned())),
                    "document" => Ok(JsVal::NativeHandle("document".to_owned())),
                    "location" => Ok(JsVal::NativeHandle("location".to_owned())),
                    "localStorage" => Ok(JsVal::NativeHandle("localStorage".to_owned())),
                    "navigator" => Ok(JsVal::NativeHandle("navigator".to_owned())),
                    "performance" => Ok(JsVal::NativeHandle("performance".to_owned())),
                    "crypto" => Ok(JsVal::NativeHandle("crypto".to_owned())),
                    "chrome" => {
                        let chrome = vm.globals.get("chrome").cloned();
                        Ok(chrome.unwrap_or(JsVal::Undefined))
                    }
                    "innerWidth" => Ok(JsVal::Number(self.viewport_width as f64)),
                    "innerHeight" => Ok(JsVal::Number(720.0)),
                    "devicePixelRatio" => Ok(JsVal::Number(1.0)),
                    "scrollX" | "pageXOffset" => Ok(JsVal::Number(self.scroll_x)),
                    "scrollY" | "pageYOffset" => Ok(JsVal::Number(self.scroll_y)),
                    "closed" => Ok(JsVal::Bool(false)),
                    "name" => Ok(JsVal::Str(String::new())),
                    "event" => Ok(JsVal::Undefined),
                    _ => Ok(JsVal::Undefined),
                },
                "navigator" => {
                    let profile = wisp_net::StealthProfile::for_policy(&self.policy);
                    Ok(match prop {
                        "userAgent" => JsVal::Str(profile.user_agent.to_owned()),
                        "language" => JsVal::Str("en-US".to_owned()),
                        "languages" => JsVal::Array(vec![
                            JsVal::Str("en-US".to_owned()),
                            JsVal::Str("en".to_owned()),
                        ]),
                        "webdriver" => JsVal::Bool(false),
                        "cookieEnabled" => JsVal::Bool(true),
                        "platform" => JsVal::Str(if self.policy.stealth {
                            "Win32".to_owned()
                        } else {
                            "Wisp".to_owned()
                        }),
                        "hardwareConcurrency" => JsVal::Number(8.0),
                        "plugins" => JsVal::Array(if self.policy.stealth {
                            vec![JsVal::Str("PDF Viewer".to_owned())]
                        } else {
                            Vec::new()
                        }),
                        _ => JsVal::Undefined,
                    })
                }
                "location" => Ok(JsVal::Str(location_part(self.page_url.as_deref(), prop))),
                _ if handle.starts_with("url:") => {
                    let raw = &handle[4..];
                    if prop == "searchParams" {
                        Ok(JsVal::NativeHandle(format!("searchParams:{raw}")))
                    } else {
                        Ok(JsVal::Str(location_part(Some(raw), prop)))
                    }
                }
                _ if handle.starts_with("style:") => {
                    let Ok(nid) = handle[6..].parse::<usize>() else {
                        return Ok(JsVal::Undefined);
                    };
                    let kebab = camel_to_kebab(prop);
                    let val = match &self.nodes.get(nid).map(|n| &n.kind) {
                        Some(NodeKind::Element { styles, .. }) => styles
                            .iter()
                            .find(|(k, _)| k == &kebab)
                            .map(|(_, v)| v.clone())
                            .unwrap_or_default(),
                        _ => String::new(),
                    };
                    Ok(JsVal::Str(val))
                }
                _ if handle.starts_with("cstyle:") => {
                    let Ok(nid) = handle[7..].parse::<usize>() else {
                        return Ok(JsVal::Undefined);
                    };
                    self.ensure_geometry();
                    let kebab = camel_to_kebab(prop);
                    if let Some(geom) = self.geometry_cache.get(&nid) {
                        let val = match kebab.as_str() {
                            "display" => geom.display.clone(),
                            "visibility" => geom.visibility.clone(),
                            "position" => geom.position.clone(),
                            "color" => geom.color.clone(),
                            "background-color" => geom.background_color.clone(),
                            "font-size" => geom.font_size.clone(),
                            "width" => format!("{}px", geom.width),
                            "height" => format!("{}px", geom.height),
                            "top" => format!("{}px", geom.y),
                            "left" => format!("{}px", geom.x),
                            _ => match &self.nodes.get(nid).map(|n| &n.kind) {
                                Some(NodeKind::Element { styles, .. }) => styles
                                    .iter()
                                    .find(|(k, _)| k == &kebab)
                                    .map(|(_, v)| v.clone())
                                    .unwrap_or_default(),
                                _ => String::new(),
                            },
                        };
                        return Ok(JsVal::Str(val));
                    }
                    Ok(JsVal::Str(String::new()))
                }
                _ => Ok(JsVal::Undefined),
            },
            JsVal::DomNode(nid) => {
                if matches!(
                    prop,
                    "offsetWidth"
                        | "offsetHeight"
                        | "offsetTop"
                        | "offsetLeft"
                        | "clientWidth"
                        | "clientHeight"
                        | "scrollWidth"
                        | "scrollHeight"
                ) {
                    self.ensure_geometry();
                    let geom = self.geometry_cache.get(nid).cloned().unwrap_or_default();
                    let num = match prop {
                        "offsetWidth" | "clientWidth" | "scrollWidth" => geom.width as f64,
                        "offsetHeight" | "clientHeight" | "scrollHeight" => geom.height as f64,
                        "offsetTop" => geom.y as f64,
                        "offsetLeft" => geom.x as f64,
                        _ => 0.0,
                    };
                    return Ok(JsVal::Number(num));
                }
                let Some(node) = self.nodes.get(*nid) else {
                    return Ok(JsVal::Undefined);
                };
                match prop {
                    "textContent" | "innerText" | "outerText" => {
                        let mut text = String::new();
                        self.collect_text(*nid, &mut text);
                        Ok(JsVal::Str(text))
                    }
                    "innerHTML" => {
                        let mut out = String::new();
                        for &c in &node.children {
                            self.serialize_node(c, &mut out);
                        }
                        Ok(JsVal::Str(out))
                    }
                    "outerHTML" => {
                        let mut out = String::new();
                        self.serialize_node(*nid, &mut out);
                        Ok(JsVal::Str(out))
                    }
                    "nodeType" => Ok(JsVal::Number(match &node.kind {
                        NodeKind::Element { .. } => 1.0,
                        NodeKind::Text(_) => 3.0,
                        NodeKind::Document => 9.0,
                    })),
                    "nodeValue" => Ok(match &node.kind {
                        NodeKind::Text(t) => JsVal::Str(t.clone()),
                        _ => JsVal::Null,
                    }),
                    "tagName" | "nodeName" => Ok(match &node.kind {
                        NodeKind::Element { tag, .. } => JsVal::Str(tag.to_ascii_uppercase()),
                        NodeKind::Document => JsVal::Str("#document".to_owned()),
                        NodeKind::Text(_) => JsVal::Str("#text".to_owned()),
                    }),
                    "id" => Ok(match &node.kind {
                        NodeKind::Element { attrs, .. } => {
                            JsVal::Str(attrs.get("id").cloned().unwrap_or_default())
                        }
                        _ => JsVal::Str(String::new()),
                    }),
                    "className" => Ok(match &node.kind {
                        NodeKind::Element { attrs, .. } => {
                            JsVal::Str(attrs.get("class").cloned().unwrap_or_default())
                        }
                        _ => JsVal::Str(String::new()),
                    }),
                    "title" | "lang" | "dir" | "accessKey" => Ok(match &node.kind {
                        NodeKind::Element { attrs, .. } => {
                            let key = if prop == "accessKey" {
                                "accesskey"
                            } else {
                                prop
                            };
                            JsVal::Str(attrs.get(key).cloned().unwrap_or_default())
                        }
                        _ => JsVal::Str(String::new()),
                    }),
                    "hidden" => Ok(match &node.kind {
                        NodeKind::Element { attrs, .. } => {
                            JsVal::Bool(attrs.contains_key("hidden"))
                        }
                        _ => JsVal::Bool(false),
                    }),
                    "classList" => Ok(JsVal::NativeHandle(format!("classList:{nid}"))),
                    "style" => Ok(JsVal::NativeHandle(format!("style:{nid}"))),
                    "attributes" => Ok(match &node.kind {
                        NodeKind::Element { attrs, .. } => {
                            let pairs = attrs
                                .iter()
                                .map(|(k, v)| (k.clone(), JsVal::Str(v.clone())))
                                .collect();
                            vm.alloc_object(pairs)
                        }
                        _ => vm.alloc_object(HashMap::new()),
                    }),
                    "childNodes" => Ok(JsVal::Array(
                        node.children.iter().copied().map(JsVal::DomNode).collect(),
                    )),
                    "firstChild" => Ok(node
                        .children
                        .first()
                        .copied()
                        .map(JsVal::DomNode)
                        .unwrap_or(JsVal::Null)),
                    "lastChild" => Ok(node
                        .children
                        .last()
                        .copied()
                        .map(JsVal::DomNode)
                        .unwrap_or(JsVal::Null)),
                    "previousSibling" | "nextSibling" => {
                        let sibling = node.parent.and_then(|pid| {
                            let siblings = &self.nodes.get(pid)?.children;
                            let pos = siblings.iter().position(|&c| c == *nid)?;
                            if prop == "previousSibling" {
                                pos.checked_sub(1).and_then(|i| siblings.get(i).copied())
                            } else {
                                siblings.get(pos + 1).copied()
                            }
                        });
                        Ok(sibling.map(JsVal::DomNode).unwrap_or(JsVal::Null))
                    }
                    "parentNode" | "parentElement" => {
                        Ok(node.parent.map(JsVal::DomNode).unwrap_or(JsVal::Null))
                    }
                    "children" => {
                        let kids: Vec<JsVal> = node
                            .children
                            .iter()
                            .copied()
                            .filter(|&c| matches!(self.nodes[c].kind, NodeKind::Element { .. }))
                            .map(JsVal::DomNode)
                            .collect();
                        Ok(JsVal::Array(kids))
                    }
                    _ => {
                        let _ = vm;
                        Ok(JsVal::Undefined)
                    }
                }
            }
            _ => Ok(JsVal::Undefined),
        }
    }

    fn set_prop(
        &mut self,
        _vm: &mut VmRuntime,
        target: &JsVal,
        prop: &str,
        value: JsVal,
    ) -> Result<(), JsVal> {
        match target {
            JsVal::NativeHandle(handle) => {
                if handle == "document" && prop == "cookie" {
                    let raw = value.to_string();
                    let parsed = self
                        .page_url
                        .as_deref()
                        .and_then(|u| url::Url::parse(u).ok());
                    let (host, path) = match parsed {
                        Some(ref u) => (
                            u.host_str().unwrap_or("localhost").to_owned(),
                            u.path().to_owned(),
                        ),
                        None => ("localhost".to_owned(), "/".to_owned()),
                    };
                    self.cookie_jar.set_from_header(&raw, &host, &path);
                } else if handle == "document" && prop == "title" {
                    let title_text = value.to_string();
                    if let Some(tid) = self.find_first_tag(self.doc_id, "title") {
                        self.set_text_content(tid, title_text);
                    } else {
                        let title_el = self.alloc_node(NodeKind::Element {
                            tag: "title".to_owned(),
                            attrs: HashMap::new(),
                            styles: Vec::new(),
                        });
                        let txt = self.alloc_node(NodeKind::Text(title_text));
                        self.attach_child(title_el, txt);
                        let parent = self.head_id.or(self.html_id).unwrap_or(self.doc_id);
                        self.attach_child(parent, title_el);
                    }
                    self.mark_mutated(self.doc_id, "childList", None);
                } else if let Some(rest) = handle.strip_prefix("style:")
                    && let Ok(nid) = rest.parse::<usize>()
                    && let Some(node) = self.nodes.get_mut(nid)
                    && let NodeKind::Element { styles, .. } = &mut node.kind
                {
                    let key = camel_to_kebab(prop);
                    let val = value.to_string();
                    if let Some((_, existing)) = styles.iter_mut().find(|(k, _)| k == &key) {
                        *existing = val;
                    } else {
                        styles.push((key, val));
                    }
                    self.mark_mutated(nid, "attributes", Some("style"));
                }
            }
            JsVal::DomNode(nid) => match prop {
                "textContent" | "innerText" => {
                    self.set_text_content(*nid, value.to_string());
                    self.mark_mutated(*nid, "childList", None);
                }
                "innerHTML" => {
                    self.set_inner_html(*nid, &value.to_string());
                    self.mark_mutated(*nid, "childList", None);
                }
                "id" => {
                    if let Some(node) = self.nodes.get_mut(*nid)
                        && let NodeKind::Element { attrs, .. } = &mut node.kind
                    {
                        attrs.insert("id".to_owned(), value.to_string());
                        self.mark_mutated(*nid, "attributes", Some("id"));
                    }
                }
                "className" => {
                    if let Some(node) = self.nodes.get_mut(*nid)
                        && let NodeKind::Element { attrs, .. } = &mut node.kind
                    {
                        attrs.insert("class".to_owned(), value.to_string());
                        self.mark_mutated(*nid, "attributes", Some("class"));
                    }
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    fn call_method(
        &mut self,
        vm: &mut VmRuntime,
        target: &JsVal,
        method: &str,
        args: Vec<JsVal>,
    ) -> Result<JsVal, JsVal> {
        match target {
            JsVal::NativeHandle(handle) => match handle.as_str() {
                "document" => match method {
                    "getElementById" => {
                        let id = args.first().map(ToString::to_string).unwrap_or_default();
                        Ok(self
                            .get_element_by_id(&id)
                            .map(JsVal::DomNode)
                            .unwrap_or(JsVal::Null))
                    }
                    "querySelector" => {
                        let sel = args.first().map(ToString::to_string).unwrap_or_default();
                        Ok(self
                            .query_selector_all(self.doc_id, &sel)
                            .into_iter()
                            .next()
                            .map(JsVal::DomNode)
                            .unwrap_or(JsVal::Null))
                    }
                    "querySelectorAll" => {
                        let sel = args.first().map(ToString::to_string).unwrap_or_default();
                        let list = self
                            .query_selector_all(self.doc_id, &sel)
                            .into_iter()
                            .map(JsVal::DomNode)
                            .collect();
                        Ok(JsVal::Array(list))
                    }
                    "getElementsByClassName" => {
                        let cls = args.first().map(ToString::to_string).unwrap_or_default();
                        let list = self
                            .get_elements_by_class_name(&cls)
                            .into_iter()
                            .map(JsVal::DomNode)
                            .collect();
                        Ok(JsVal::Array(list))
                    }
                    "getElementsByTagName" => {
                        let tag = args.first().map(ToString::to_string).unwrap_or_default();
                        let list = self
                            .get_elements_by_tag_name(&tag)
                            .into_iter()
                            .map(JsVal::DomNode)
                            .collect();
                        Ok(JsVal::Array(list))
                    }
                    "createElement" => {
                        let tag = args
                            .first()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| "div".to_owned())
                            .to_ascii_lowercase();
                        let id = self.alloc_node(NodeKind::Element {
                            tag,
                            attrs: HashMap::new(),
                            styles: Vec::new(),
                        });
                        Ok(JsVal::DomNode(id))
                    }
                    "createTextNode" => {
                        let txt = args.first().map(ToString::to_string).unwrap_or_default();
                        Ok(JsVal::DomNode(self.alloc_node(NodeKind::Text(txt))))
                    }
                    "createDocumentFragment" => {
                        let id = self.alloc_node(NodeKind::Element {
                            tag: "fragment".to_owned(),
                            attrs: HashMap::new(),
                            styles: Vec::new(),
                        });
                        Ok(JsVal::DomNode(id))
                    }
                    "open" | "close" => Ok(JsVal::Undefined),
                    "write" => {
                        let html: String = args.iter().map(ToString::to_string).collect();
                        if let Some(body) = self.body_id {
                            self.insert_adjacent_html(body, "beforeend", &html);
                        }
                        Ok(JsVal::Undefined)
                    }
                    "addEventListener" => {
                        if let (Some(JsVal::Str(ev)), Some(JsVal::Function(fn_id))) =
                            (args.first(), args.get(1))
                        {
                            self.doc_listeners
                                .entry(ev.clone())
                                .or_default()
                                .push(*fn_id);
                        }
                        Ok(JsVal::Undefined)
                    }
                    "removeEventListener" => Ok(JsVal::Bool(true)),
                    "dispatchEvent" => {
                        let ev = args.into_iter().next().unwrap_or(JsVal::Undefined);
                        let ev_type = match &ev {
                            JsVal::Object(id) => vm
                                .objects
                                .get(*id)
                                .and_then(|o| o.get("type"))
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                            _ => String::new(),
                        };
                        if let Some(fns) = self.doc_listeners.get(&ev_type).cloned() {
                            for fn_id in fns {
                                let _ = vm.invoke_fn(fn_id, vec![ev.clone()], self);
                            }
                        }
                        Ok(JsVal::Bool(true))
                    }
                    "elementFromPoint" => {
                        let x = args.first().map(JsVal::to_number).unwrap_or(0.0) as f32;
                        let y = args.get(1).map(JsVal::to_number).unwrap_or(0.0) as f32;
                        self.ensure_geometry();
                        let mut preorder = Vec::new();
                        self.collect_preorder_elements(self.doc_id, &mut preorder);
                        let mut hit = None;
                        for nid in preorder {
                            if let Some(geom) = self.geometry_cache.get(&nid)
                                && geom.display != "none"
                                && geom.width > 0.0
                                && geom.height > 0.0
                                && x >= geom.x
                                && x <= geom.x + geom.width
                                && y >= geom.y
                                && y <= geom.y + geom.height
                            {
                                hit = Some(nid);
                            }
                        }
                        Ok(hit.map(JsVal::DomNode).unwrap_or(JsVal::Null))
                    }
                    _ => Err(JsVal::Str(format!("document.{method} no soportado"))),
                },
                "window" => match method {
                    "addEventListener" => {
                        if let (Some(JsVal::Str(ev)), Some(JsVal::Function(fn_id))) =
                            (args.first(), args.get(1))
                        {
                            self.win_listeners
                                .entry(ev.clone())
                                .or_default()
                                .push(*fn_id);
                        }
                        Ok(JsVal::Undefined)
                    }
                    "removeEventListener" => Ok(JsVal::Bool(true)),
                    "dispatchEvent" => {
                        let ev = args.into_iter().next().unwrap_or(JsVal::Undefined);
                        let ev_type = match &ev {
                            JsVal::Object(id) => vm
                                .objects
                                .get(*id)
                                .and_then(|o| o.get("type"))
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                            _ => String::new(),
                        };
                        if let Some(fns) = self.win_listeners.get(&ev_type).cloned() {
                            for fn_id in fns {
                                let _ = vm.invoke_fn(fn_id, vec![ev.clone()], self);
                            }
                        }
                        Ok(JsVal::Bool(true))
                    }
                    "getComputedStyle" => {
                        if let Some(JsVal::DomNode(nid)) = args.first() {
                            Ok(JsVal::NativeHandle(format!("cstyle:{nid}")))
                        } else {
                            Ok(vm.alloc_object(HashMap::new()))
                        }
                    }
                    "scrollTo" | "scroll" => {
                        self.scroll_x = args.first().map(JsVal::to_number).unwrap_or(0.0);
                        self.scroll_y = args.get(1).map(JsVal::to_number).unwrap_or(0.0);
                        Ok(JsVal::Undefined)
                    }
                    "alert" | "close" | "focus" | "blur" => Ok(JsVal::Undefined),
                    "open" => Ok(JsVal::Null),
                    _ => Ok(JsVal::Undefined),
                },
                "performance" => match method {
                    "now" => Ok(JsVal::Number(self.started_at.elapsed().as_secs_f64() * 1000.0)),
                    _ => Ok(JsVal::Undefined),
                },
                "crypto" => match method {
                    "getRandomValues" => {
                        if let Some(arg) = args.first() {
                            if let Some(mut buf) = vm.read_uint8_array(arg) {
                                fill_random_bytes(&mut buf);
                                let _ = vm.write_uint8_array(arg, &buf);
                                return Ok(arg.clone());
                            }
                            if let JsVal::Array(items) = arg {
                                let mut buf = vec![0u8; items.len()];
                                fill_random_bytes(&mut buf);
                                return Ok(JsVal::Array(
                                    buf.into_iter().map(|b| JsVal::Number(b as f64)).collect(),
                                ));
                            }
                        }
                        Ok(JsVal::Undefined)
                    }
                    "randomUUID" => {
                        let mut b = [0u8; 16];
                        fill_random_bytes(&mut b);
                        b[6] = (b[6] & 0x0f) | 0x40;
                        b[8] = (b[8] & 0x3f) | 0x80;
                        let uuid = format!(
                            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}\
                             -{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                            b[0],
                            b[1],
                            b[2],
                            b[3],
                            b[4],
                            b[5],
                            b[6],
                            b[7],
                            b[8],
                            b[9],
                            b[10],
                            b[11],
                            b[12],
                            b[13],
                            b[14],
                            b[15]
                        );
                        Ok(JsVal::Str(uuid))
                    }
                    _ => Ok(JsVal::Undefined),
                },
                "localStorage" => match method {
                    "getItem" => {
                        let k = args.first().map(ToString::to_string).unwrap_or_default();
                        Ok(self
                            .storage
                            .get(&k)
                            .cloned()
                            .map(JsVal::Str)
                            .unwrap_or(JsVal::Null))
                    }
                    "setItem" => {
                        let k = args.first().map(ToString::to_string).unwrap_or_default();
                        let v = args.get(1).map(ToString::to_string).unwrap_or_default();
                        self.storage.insert(k, v);
                        Ok(JsVal::Undefined)
                    }
                    "removeItem" => {
                        let k = args.first().map(ToString::to_string).unwrap_or_default();
                        self.storage.remove(&k);
                        Ok(JsVal::Undefined)
                    }
                    "clear" => {
                        self.storage.clear();
                        Ok(JsVal::Undefined)
                    }
                    _ => Ok(JsVal::Undefined),
                },
                "console" => Ok(JsVal::Undefined),
                "JSON" => match method {
                    "stringify" => {
                        let val = args
                            .first()
                            .and_then(|v| jsval_to_json(v, vm))
                            .unwrap_or(Value::Null);
                        Ok(JsVal::Str(val.to_string()))
                    }
                    "parse" => {
                        let raw = args.first().map(ToString::to_string).unwrap_or_default();
                        let parsed: Value = serde_json::from_str(&raw)
                            .map_err(|e| JsVal::Str(format!("JSON.parse: {e}")))?;
                        Ok(json_to_jsval(&parsed, vm))
                    }
                    _ => Ok(JsVal::Undefined),
                },
                "Promise" => match method {
                    "resolve" => {
                        let v = args.into_iter().next().unwrap_or(JsVal::Undefined);
                        Ok(self.alloc_promise(Some(v), None))
                    }
                    "reject" => {
                        let e = args.into_iter().next().unwrap_or(JsVal::Undefined);
                        Ok(self.alloc_promise(None, Some(e)))
                    }
                    _ => Ok(JsVal::Undefined),
                },
                "Array" => match method {
                    "isArray" => Ok(JsVal::Bool(matches!(args.first(), Some(JsVal::Array(_))))),
                    "from" => match args.first() {
                        Some(JsVal::Array(items)) => Ok(JsVal::Array(items.clone())),
                        Some(JsVal::Str(s)) => Ok(JsVal::Array(
                            s.chars().map(|c| JsVal::Str(c.to_string())).collect(),
                        )),
                        Some(other) => {
                            if let Some(buf) = vm.read_uint8_array(other) {
                                let items = buf.iter().map(|&b| JsVal::Number(b as f64)).collect();
                                Ok(JsVal::Array(items))
                            } else {
                                Ok(JsVal::Array(Vec::new()))
                            }
                        }
                        None => Ok(JsVal::Array(Vec::new())),
                    },
                    _ => Ok(JsVal::Undefined),
                },
                "TextEncoder" => match method {
                    "encode" => {
                        let s = args.first().map(ToString::to_string).unwrap_or_default();
                        Ok(vm.alloc_uint8_array(s.into_bytes()))
                    }
                    _ => Ok(JsVal::Undefined),
                },
                "TextDecoder" => match method {
                    "decode" => {
                        let bytes: Vec<u8> = match args.first() {
                            Some(JsVal::Array(items)) => {
                                items.iter().map(|v| v.to_number() as u8).collect()
                            }
                            Some(other) => vm.read_uint8_array(other).unwrap_or_default(),
                            None => Vec::new(),
                        };
                        Ok(JsVal::Str(String::from_utf8_lossy(&bytes).into_owned()))
                    }
                    _ => Ok(JsVal::Undefined),
                },
                _ if handle.starts_with("promise:") => {
                    let Ok(pid) = handle[8..].parse::<usize>() else {
                        return Ok(JsVal::Undefined);
                    };
                    let state = self.promises.get(pid).cloned();
                    match (method, state) {
                        (
                            "then",
                            Some(PromiseState {
                                resolved: Some(val),
                                ..
                            }),
                        ) => {
                            let next = self.alloc_promise(None, None);
                            let JsVal::NativeHandle(nh) = &next else {
                                unreachable!();
                            };
                            let next_id = nh[8..].parse::<usize>().unwrap_or(0);
                            if let Some(JsVal::Function(fn_id)) = args.first() {
                                self.microtasks.push(Microtask {
                                    cb_fn_id: *fn_id,
                                    arg: val,
                                    target_promise: next_id,
                                });
                            } else {
                                self.settle_promise(next_id, true, val);
                            }
                            Ok(next)
                        }
                        (
                            "then",
                            Some(PromiseState {
                                rejected: Some(err),
                                ..
                            }),
                        ) => {
                            let next = self.alloc_promise(None, None);
                            let JsVal::NativeHandle(nh) = &next else {
                                unreachable!();
                            };
                            let next_id = nh[8..].parse::<usize>().unwrap_or(0);
                            if let Some(JsVal::Function(fn_id)) = args.get(1) {
                                self.microtasks.push(Microtask {
                                    cb_fn_id: *fn_id,
                                    arg: err,
                                    target_promise: next_id,
                                });
                            } else {
                                self.settle_promise(next_id, false, err);
                            }
                            Ok(next)
                        }
                        (
                            "then",
                            Some(PromiseState {
                                resolved: None,
                                rejected: None,
                                ..
                            }),
                        ) => {
                            let next = self.alloc_promise(None, None);
                            let JsVal::NativeHandle(nh) = &next else {
                                unreachable!();
                            };
                            let next_id = nh[8..].parse::<usize>().unwrap_or(0);
                            let cb = match args.first() {
                                Some(JsVal::Function(fn_id)) => Some(*fn_id),
                                _ => None,
                            };
                            if let Some(p) = self.promises.get_mut(pid) {
                                p.waiters.push(PromiseWaiter {
                                    cb_fn_id: cb,
                                    target_promise: next_id,
                                    is_catch: false,
                                });
                            }
                            Ok(next)
                        }
                        (
                            "catch",
                            Some(PromiseState {
                                resolved: Some(val),
                                ..
                            }),
                        ) => Ok(self.alloc_promise(Some(val), None)),
                        (
                            "catch",
                            Some(PromiseState {
                                rejected: Some(err),
                                ..
                            }),
                        ) => {
                            let next = self.alloc_promise(None, None);
                            let JsVal::NativeHandle(nh) = &next else {
                                unreachable!();
                            };
                            let next_id = nh[8..].parse::<usize>().unwrap_or(0);
                            if let Some(JsVal::Function(fn_id)) = args.first() {
                                self.microtasks.push(Microtask {
                                    cb_fn_id: *fn_id,
                                    arg: err,
                                    target_promise: next_id,
                                });
                            } else {
                                self.settle_promise(next_id, false, err);
                            }
                            Ok(next)
                        }
                        (
                            "catch",
                            Some(PromiseState {
                                resolved: None,
                                rejected: None,
                                ..
                            }),
                        ) => {
                            let next = self.alloc_promise(None, None);
                            let JsVal::NativeHandle(nh) = &next else {
                                unreachable!();
                            };
                            let next_id = nh[8..].parse::<usize>().unwrap_or(0);
                            let cb = match args.first() {
                                Some(JsVal::Function(fn_id)) => Some(*fn_id),
                                _ => None,
                            };
                            if let Some(p) = self.promises.get_mut(pid) {
                                p.waiters.push(PromiseWaiter {
                                    cb_fn_id: cb,
                                    target_promise: next_id,
                                    is_catch: true,
                                });
                            }
                            Ok(next)
                        }
                        _ => Ok(target.clone()),
                    }
                }
                _ if handle.starts_with("searchParams:") => {
                    let raw = &handle[13..];
                    if method == "get" {
                        let key = args.first().map(ToString::to_string).unwrap_or_default();
                        if let Ok(parsed) = url::Url::parse(raw)
                            && let Some((_, v)) = parsed.query_pairs().find(|(k, _)| k == &key)
                        {
                            return Ok(JsVal::Str(v.into_owned()));
                        }
                        Ok(JsVal::Null)
                    } else {
                        Ok(JsVal::Undefined)
                    }
                }
                _ if handle.starts_with("classList:") => {
                    let Ok(nid) = handle[10..].parse::<usize>() else {
                        return Ok(JsVal::Undefined);
                    };
                    let cls = args.first().map(ToString::to_string).unwrap_or_default();
                    let mut class_mutated = false;
                    let result = {
                        let Some(node) = self.nodes.get_mut(nid) else {
                            return Ok(JsVal::Undefined);
                        };
                        let NodeKind::Element { attrs, .. } = &mut node.kind else {
                            return Ok(JsVal::Undefined);
                        };
                        let mut list: Vec<String> = attrs
                            .get("class")
                            .map(|s| s.split_whitespace().map(str::to_owned).collect())
                            .unwrap_or_default();
                        match method {
                            "add" => {
                                if !cls.is_empty() && !list.contains(&cls) {
                                    list.push(cls);
                                    attrs.insert("class".to_owned(), list.join(" "));
                                    class_mutated = true;
                                }
                                JsVal::Undefined
                            }
                            "remove" => {
                                list.retain(|c| c != &cls);
                                attrs.insert("class".to_owned(), list.join(" "));
                                class_mutated = true;
                                JsVal::Undefined
                            }
                            "contains" => JsVal::Bool(list.contains(&cls)),
                            "toggle" => {
                                let added = if list.contains(&cls) {
                                    list.retain(|c| c != &cls);
                                    false
                                } else {
                                    list.push(cls);
                                    true
                                };
                                attrs.insert("class".to_owned(), list.join(" "));
                                class_mutated = true;
                                JsVal::Bool(added)
                            }
                            _ => JsVal::Undefined,
                        }
                    };
                    if class_mutated {
                        self.mark_mutated(nid, "attributes", Some("class"));
                    }
                    Ok(result)
                }
                _ if handle.starts_with("observer:") => {
                    let Ok(oid) = handle[9..].parse::<usize>() else {
                        return Ok(JsVal::Undefined);
                    };
                    match method {
                        "observe" => {
                            if let Some(JsVal::DomNode(target_nid)) = args.first() {
                                let (subtree, attributes, child_list) = match args.get(1) {
                                    Some(JsVal::Object(obj_id)) => {
                                        let obj = vm.objects.get(*obj_id);
                                        let sub = obj
                                            .and_then(|m| m.get("subtree"))
                                            .is_some_and(JsVal::is_truthy);
                                        let attrs = obj
                                            .and_then(|m| m.get("attributes"))
                                            .is_some_and(JsVal::is_truthy);
                                        let cl = obj
                                            .and_then(|m| m.get("childList"))
                                            .is_some_and(JsVal::is_truthy);
                                        (sub, attrs, cl || !attrs)
                                    }
                                    _ => (false, false, true),
                                };
                                if let Some(obs) = self.observers.get_mut(oid) {
                                    if !obs.targets.contains(target_nid) {
                                        obs.targets.push(*target_nid);
                                    }
                                    obs.subtree |= subtree;
                                    obs.attributes |= attributes;
                                    obs.child_list |= child_list;
                                }
                            }
                            Ok(JsVal::Undefined)
                        }
                        "disconnect" => {
                            if let Some(obs) = self.observers.get_mut(oid) {
                                obs.targets.clear();
                                obs.records.clear();
                            }
                            Ok(JsVal::Undefined)
                        }
                        "takeRecords" => {
                            let recs = self
                                .observers
                                .get_mut(oid)
                                .map(|obs| std::mem::take(&mut obs.records))
                                .unwrap_or_default();
                            let js_recs: Vec<JsVal> = recs
                                .into_iter()
                                .map(|r| {
                                    vm.alloc_object(HashMap::from([
                                        ("type".to_owned(), JsVal::Str(r.kind)),
                                        ("target".to_owned(), JsVal::DomNode(r.target)),
                                        (
                                            "attributeName".to_owned(),
                                            r.attribute_name.map(JsVal::Str).unwrap_or(JsVal::Null),
                                        ),
                                    ]))
                                })
                                .collect();
                            Ok(JsVal::Array(js_recs))
                        }
                        _ => Ok(JsVal::Undefined),
                    }
                }
                _ if handle.starts_with("cstyle:") => {
                    let Ok(nid) = handle[7..].parse::<usize>() else {
                        return Ok(JsVal::Undefined);
                    };
                    if method == "getPropertyValue" {
                        let key = args
                            .first()
                            .map(ToString::to_string)
                            .unwrap_or_default()
                            .to_ascii_lowercase();
                        return self.get_prop(
                            vm,
                            &JsVal::NativeHandle(format!("cstyle:{nid}")),
                            &key,
                        );
                    }
                    Ok(JsVal::Undefined)
                }
                _ if handle.starts_with("style:") => {
                    let Ok(nid) = handle[6..].parse::<usize>() else {
                        return Ok(JsVal::Undefined);
                    };
                    match method {
                        "setProperty" => {
                            let key = args
                                .first()
                                .map(ToString::to_string)
                                .unwrap_or_default()
                                .to_ascii_lowercase();
                            let val = args.get(1).map(ToString::to_string).unwrap_or_default();
                            let mut changed = false;
                            if let Some(node) = self.nodes.get_mut(nid)
                                && let NodeKind::Element { styles, .. } = &mut node.kind
                            {
                                if let Some((_, existing)) =
                                    styles.iter_mut().find(|(k, _)| k == &key)
                                {
                                    *existing = val;
                                } else {
                                    styles.push((key, val));
                                }
                                changed = true;
                            }
                            if changed {
                                self.mark_mutated(nid, "attributes", Some("style"));
                            }
                            Ok(JsVal::Undefined)
                        }
                        "getPropertyValue" => {
                            let key = args
                                .first()
                                .map(ToString::to_string)
                                .unwrap_or_default()
                                .to_ascii_lowercase();
                            let val = match self.nodes.get(nid).map(|n| &n.kind) {
                                Some(NodeKind::Element { styles, .. }) => styles
                                    .iter()
                                    .find(|(k, _)| k == &key)
                                    .map(|(_, v)| v.clone())
                                    .unwrap_or_default(),
                                _ => String::new(),
                            };
                            Ok(JsVal::Str(val))
                        }
                        _ => Ok(JsVal::Undefined),
                    }
                }
                _ => Ok(JsVal::Undefined),
            },
            JsVal::DomNode(nid) => match method {
                "getAttribute" => {
                    let name = args.first().map(ToString::to_string).unwrap_or_default();
                    let val = match self.nodes.get(*nid).map(|n| &n.kind) {
                        Some(NodeKind::Element { attrs, .. }) => attrs.get(&name).cloned(),
                        _ => None,
                    };
                    Ok(val.map(JsVal::Str).unwrap_or(JsVal::Null))
                }
                "setAttribute" => {
                    let name = args.first().map(ToString::to_string).unwrap_or_default();
                    let val = args.get(1).map(ToString::to_string).unwrap_or_default();
                    let mut changed = false;
                    if let Some(node) = self.nodes.get_mut(*nid)
                        && let NodeKind::Element { attrs, .. } = &mut node.kind
                    {
                        attrs.insert(name.clone(), val);
                        changed = true;
                    }
                    if changed {
                        self.mark_mutated(*nid, "attributes", Some(&name));
                    }
                    Ok(JsVal::Undefined)
                }
                "removeAttribute" => {
                    let name = args.first().map(ToString::to_string).unwrap_or_default();
                    let mut changed = false;
                    if let Some(node) = self.nodes.get_mut(*nid)
                        && let NodeKind::Element { attrs, .. } = &mut node.kind
                    {
                        attrs.remove(&name);
                        changed = true;
                    }
                    if changed {
                        self.mark_mutated(*nid, "attributes", Some(&name));
                    }
                    Ok(JsVal::Undefined)
                }
                "hasAttribute" => {
                    let name = args.first().map(ToString::to_string).unwrap_or_default();
                    let has = match self.nodes.get(*nid).map(|n| &n.kind) {
                        Some(NodeKind::Element { attrs, .. }) => attrs.contains_key(&name),
                        _ => false,
                    };
                    Ok(JsVal::Bool(has))
                }
                "toggleAttribute" => {
                    let name = args.first().map(ToString::to_string).unwrap_or_default();
                    let force = args.get(1).map(JsVal::is_truthy);
                    let mut present = false;
                    let mut changed = false;
                    if let Some(node) = self.nodes.get_mut(*nid)
                        && let NodeKind::Element { attrs, .. } = &mut node.kind
                    {
                        let had = attrs.contains_key(&name);
                        let want = force.unwrap_or(!had);
                        if want {
                            attrs.entry(name.clone()).or_default();
                            present = true;
                        } else {
                            attrs.remove(&name);
                        }
                        changed = true;
                    }
                    if changed {
                        self.mark_mutated(*nid, "attributes", Some(&name));
                    }
                    Ok(JsVal::Bool(present))
                }
                "appendChild" => {
                    if let Some(JsVal::DomNode(child)) = args.first() {
                        self.attach_child(*nid, *child);
                        self.mark_mutated(*nid, "childList", None);
                        Ok(JsVal::DomNode(*child))
                    } else {
                        Ok(JsVal::Null)
                    }
                }
                "insertBefore" => {
                    if let Some(JsVal::DomNode(new_child)) = args.first() {
                        self.attach_child(*nid, *new_child);
                        self.mark_mutated(*nid, "childList", None);
                        Ok(JsVal::DomNode(*new_child))
                    } else {
                        Ok(JsVal::Null)
                    }
                }
                "replaceChild" => {
                    if let (Some(JsVal::DomNode(new_c)), Some(JsVal::DomNode(old_c))) =
                        (args.first(), args.get(1))
                    {
                        self.detach_node(*old_c);
                        self.attach_child(*nid, *new_c);
                        self.mark_mutated(*nid, "childList", None);
                        Ok(JsVal::DomNode(*old_c))
                    } else {
                        Ok(JsVal::Null)
                    }
                }
                "cloneNode" => {
                    let cloned = self
                        .nodes
                        .get(*nid)
                        .map(|n| n.kind.clone())
                        .map(|kind| self.alloc_node(kind));
                    Ok(cloned.map(JsVal::DomNode).unwrap_or(JsVal::Null))
                }
                "hasChildNodes" => Ok(JsVal::Bool(
                    self.nodes.get(*nid).is_some_and(|n| !n.children.is_empty()),
                )),
                "contains" => {
                    let target_id = match args.first() {
                        Some(JsVal::DomNode(other)) => Some(*other),
                        _ => None,
                    };
                    let mut cur = target_id;
                    let mut found = false;
                    while let Some(c) = cur {
                        if c == *nid {
                            found = true;
                            break;
                        }
                        cur = self.nodes.get(c).and_then(|n| n.parent);
                    }
                    Ok(JsVal::Bool(found))
                }
                "isEqualNode" => Ok(JsVal::Bool(matches!(
                    args.first(),
                    Some(JsVal::DomNode(other)) if other == nid
                ))),
                "closest" => {
                    let sel = args.first().map(ToString::to_string).unwrap_or_default();
                    let mut cur = Some(*nid);
                    let mut hit = None;
                    while let Some(c) = cur {
                        if sel
                            .split(',')
                            .map(str::trim)
                            .any(|b| self.matches_complex_selector(c, b))
                        {
                            hit = Some(c);
                            break;
                        }
                        cur = self.nodes.get(c).and_then(|n| n.parent);
                    }
                    Ok(hit.map(JsVal::DomNode).unwrap_or(JsVal::Null))
                }
                "focus" | "blur" => Ok(JsVal::Undefined),
                "removeEventListener" => Ok(JsVal::Bool(true)),
                "getBoundingClientRect" => {
                    self.ensure_geometry();
                    let geom = self.geometry_cache.get(nid).cloned().unwrap_or_default();
                    let rect = vm.alloc_object(HashMap::from([
                        ("x".to_owned(), JsVal::Number(geom.x as f64)),
                        ("y".to_owned(), JsVal::Number(geom.y as f64)),
                        ("left".to_owned(), JsVal::Number(geom.x as f64)),
                        ("top".to_owned(), JsVal::Number(geom.y as f64)),
                        ("width".to_owned(), JsVal::Number(geom.width as f64)),
                        ("height".to_owned(), JsVal::Number(geom.height as f64)),
                        ("right".to_owned(), JsVal::Number((geom.x + geom.width) as f64)),
                        ("bottom".to_owned(), JsVal::Number((geom.y + geom.height) as f64)),
                    ]));
                    Ok(rect)
                }
                "dispatchEvent" => {
                    let ev = args.into_iter().next().unwrap_or(JsVal::Undefined);
                    let (ev_type, bubbles) = match &ev {
                        JsVal::Object(id) => {
                            let obj = vm.objects.get(*id);
                            let t = obj
                                .and_then(|o| o.get("type"))
                                .map(ToString::to_string)
                                .unwrap_or_default();
                            let b = obj
                                .and_then(|o| o.get("bubbles"))
                                .is_some_and(JsVal::is_truthy);
                            (t, b)
                        }
                        _ => (String::new(), false),
                    };
                    if let JsVal::Object(id) = &ev
                        && let Some(obj) = vm.objects.get_mut(*id)
                    {
                        obj.insert("target".to_owned(), JsVal::DomNode(*nid));
                    }
                    let mut cur = Some(*nid);
                    while let Some(c) = cur {
                        if let Some(fns) = self.node_listeners.get(&(c, ev_type.clone())).cloned() {
                            if let JsVal::Object(id) = &ev
                                && let Some(obj) = vm.objects.get_mut(*id)
                            {
                                obj.insert("currentTarget".to_owned(), JsVal::DomNode(c));
                            }
                            for fn_id in fns {
                                let _ = vm.invoke_fn(fn_id, vec![ev.clone()], self);
                            }
                        }
                        if !bubbles {
                            break;
                        }
                        cur = self.nodes.get(c).and_then(|n| n.parent);
                    }
                    if bubbles && let Some(fns) = self.doc_listeners.get(&ev_type).cloned() {
                        for fn_id in fns {
                            let _ = vm.invoke_fn(fn_id, vec![ev.clone()], self);
                        }
                    }
                    Ok(JsVal::Bool(true))
                }
                "prepend" => {
                    let mut changed = false;
                    for arg in args.into_iter().rev() {
                        if let JsVal::DomNode(child) = arg {
                            self.prepend_child(*nid, child);
                            changed = true;
                        }
                    }
                    if changed {
                        self.mark_mutated(*nid, "childList", None);
                    }
                    Ok(JsVal::Undefined)
                }
                "removeChild" => {
                    if let Some(JsVal::DomNode(child)) = args.first() {
                        self.detach_node(*child);
                        self.mark_mutated(*nid, "childList", None);
                        Ok(JsVal::DomNode(*child))
                    } else {
                        Ok(JsVal::Null)
                    }
                }
                "insertAdjacentHTML" => {
                    let pos = args.first().map(ToString::to_string).unwrap_or_default();
                    let html = args.get(1).map(ToString::to_string).unwrap_or_default();
                    self.insert_adjacent_html(*nid, &pos, &html);
                    self.mark_mutated(*nid, "childList", None);
                    Ok(JsVal::Undefined)
                }
                "querySelector" => {
                    let sel = args.first().map(ToString::to_string).unwrap_or_default();
                    Ok(self
                        .query_selector_all(*nid, &sel)
                        .into_iter()
                        .next()
                        .map(JsVal::DomNode)
                        .unwrap_or(JsVal::Null))
                }
                "querySelectorAll" => {
                    let sel = args.first().map(ToString::to_string).unwrap_or_default();
                    let list = self
                        .query_selector_all(*nid, &sel)
                        .into_iter()
                        .map(JsVal::DomNode)
                        .collect();
                    Ok(JsVal::Array(list))
                }
                "addEventListener" => {
                    if let (Some(JsVal::Str(ev)), Some(JsVal::Function(fn_id))) =
                        (args.first(), args.get(1))
                    {
                        self.node_listeners
                            .entry((*nid, ev.clone()))
                            .or_default()
                            .push(*fn_id);
                    }
                    Ok(JsVal::Undefined)
                }
                "click" => {
                    if let Some(fns) = self
                        .node_listeners
                        .get(&(*nid, "click".to_owned()))
                        .cloned()
                    {
                        let ev = vm.alloc_object(HashMap::from([
                            ("type".to_owned(), JsVal::Str("click".to_owned())),
                            ("target".to_owned(), JsVal::DomNode(*nid)),
                            ("currentTarget".to_owned(), JsVal::DomNode(*nid)),
                        ]));
                        for fn_id in fns {
                            let _ = vm.invoke_fn(fn_id, vec![ev.clone()], self);
                        }
                    }
                    Ok(JsVal::Undefined)
                }
                _ => Err(JsVal::Str(format!("método DOM `{method}` no soportado"))),
            },
            JsVal::Object(id) => {
                let body_opt = vm.objects.get(*id).and_then(|m| match m.get("__wispBody") {
                    Some(JsVal::Str(s)) => Some(s.clone()),
                    _ => None,
                });
                if let Some(body) = body_opt {
                    match method {
                        "text" => return Ok(self.alloc_promise(Some(JsVal::Str(body)), None)),
                        "json" => {
                            return match serde_json::from_str::<Value>(&body) {
                                Ok(parsed) => {
                                    let val = json_to_jsval(&parsed, vm);
                                    Ok(self.alloc_promise(Some(val), None))
                                }
                                Err(err) => {
                                    let err_obj = vm.alloc_object(HashMap::from([
                                        ("name".to_owned(), JsVal::Str("SyntaxError".to_owned())),
                                        ("message".to_owned(), JsVal::Str(err.to_string())),
                                    ]));
                                    Ok(self.alloc_promise(None, Some(err_obj)))
                                }
                            };
                        }
                        _ => {}
                    }
                }
                let is_xhr = vm
                    .objects
                    .get(*id)
                    .and_then(|m| m.get("__wispXhr"))
                    .is_some_and(|v| matches!(v, JsVal::Bool(true)));
                if is_xhr {
                    match method {
                        "open" => {
                            let req_method =
                                args.first().map(ToString::to_string).unwrap_or_default();
                            let req_url = args.get(1).map(ToString::to_string).unwrap_or_default();
                            if let Some(obj) = vm.objects.get_mut(*id) {
                                obj.insert("__method".to_owned(), JsVal::Str(req_method));
                                obj.insert("__url".to_owned(), JsVal::Str(req_url));
                                obj.insert("readyState".to_owned(), JsVal::Number(1.0));
                            }
                            return Ok(JsVal::Undefined);
                        }
                        "send" => {
                            let (req_method, req_url) = vm
                                .objects
                                .get(*id)
                                .map(|obj| {
                                    (
                                        obj.get("__method")
                                            .map(ToString::to_string)
                                            .unwrap_or_else(|| "GET".to_owned()),
                                        obj.get("__url")
                                            .map(ToString::to_string)
                                            .unwrap_or_default(),
                                    )
                                })
                                .unwrap_or_else(|| ("GET".to_owned(), String::new()));
                            match fetch_json(&req_url, &req_method) {
                                Ok(json_str) => {
                                    let meta: Value =
                                        serde_json::from_str(&json_str).unwrap_or(Value::Null);
                                    let status =
                                        meta.get("status").and_then(Value::as_f64).unwrap_or(200.0);
                                    let body = meta
                                        .get("body")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned();
                                    if let Some(obj) = vm.objects.get_mut(*id) {
                                        obj.insert("status".to_owned(), JsVal::Number(status));
                                        obj.insert("readyState".to_owned(), JsVal::Number(4.0));
                                        obj.insert("responseText".to_owned(), JsVal::Str(body));
                                    }
                                    let cbs = vm
                                        .objects
                                        .get(*id)
                                        .map(|obj| {
                                            (
                                                obj.get("onreadystatechange").cloned(),
                                                obj.get("onload").cloned(),
                                            )
                                        })
                                        .unwrap_or((None, None));
                                    if let Some(JsVal::Function(fn_id)) = cbs.0 {
                                        let _ = vm.invoke_fn(fn_id, Vec::new(), self);
                                    }
                                    if let Some(JsVal::Function(fn_id)) = cbs.1 {
                                        let _ = vm.invoke_fn(fn_id, Vec::new(), self);
                                    }
                                }
                                Err(error) => {
                                    record_fetch_warning(&error);
                                    if let Some(obj) = vm.objects.get_mut(*id) {
                                        obj.insert("status".to_owned(), JsVal::Number(0.0));
                                        obj.insert("readyState".to_owned(), JsVal::Number(4.0));
                                    }
                                    if let Some(JsVal::Function(fn_id)) =
                                        vm.objects.get(*id).and_then(|o| o.get("onerror").cloned())
                                    {
                                        let _ = vm.invoke_fn(fn_id, Vec::new(), self);
                                    }
                                }
                            }
                            return Ok(JsVal::Undefined);
                        }
                        "setRequestHeader" => return Ok(JsVal::Undefined),
                        _ => {}
                    }
                }
                Err(JsVal::Str(format!("método no definido `{method}`")))
            }
            _ => Err(JsVal::Str(format!("método no definido `{method}`"))),
        }
    }

    fn call_global(
        &mut self,
        vm: &mut VmRuntime,
        name: &str,
        args: Vec<JsVal>,
    ) -> Option<Result<JsVal, JsVal>> {
        match name {
            "setTimeout" | "requestAnimationFrame" => {
                if let Some(JsVal::Function(fn_id)) = args.first() {
                    self.timers.push(*fn_id);
                }
                Some(Ok(JsVal::Number(1.0)))
            }
            "fetch" => {
                let url = args.first().map(ToString::to_string).unwrap_or_default();
                let method = args
                    .get(1)
                    .and_then(|v| match v {
                        JsVal::Object(id) => vm
                            .objects
                            .get(*id)
                            .and_then(|m| m.get("method"))
                            .map(ToString::to_string),
                        _ => None,
                    })
                    .unwrap_or_else(|| "GET".to_owned());
                match fetch_json(&url, &method) {
                    Ok(json_str) => {
                        let meta: Value = serde_json::from_str(&json_str).unwrap_or(Value::Null);
                        let status = meta.get("status").and_then(Value::as_f64).unwrap_or(200.0);
                        let ok = meta.get("ok").and_then(Value::as_bool).unwrap_or(true);
                        let final_url = meta
                            .get("url")
                            .and_then(Value::as_str)
                            .unwrap_or(&url)
                            .to_owned();
                        let body = meta
                            .get("body")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        let resp = vm.alloc_object(HashMap::from([
                            ("status".to_owned(), JsVal::Number(status)),
                            ("ok".to_owned(), JsVal::Bool(ok)),
                            ("url".to_owned(), JsVal::Str(final_url)),
                            ("__wispBody".to_owned(), JsVal::Str(body)),
                        ]));
                        Some(Ok(self.alloc_promise(Some(resp), None)))
                    }
                    Err(error) => {
                        record_fetch_warning(&error);
                        let err_obj = vm.alloc_object(HashMap::from([
                            ("name".to_owned(), JsVal::Str("TypeError".to_owned())),
                            ("message".to_owned(), JsVal::Str(error)),
                        ]));
                        Some(Ok(self.alloc_promise(None, Some(err_obj))))
                    }
                }
            }
            _ => None,
        }
    }

    fn construct(
        &mut self,
        vm: &mut VmRuntime,
        ctor: &str,
        args: Vec<JsVal>,
    ) -> Result<JsVal, JsVal> {
        match ctor {
            "Error" => {
                let msg = args.first().map(ToString::to_string).unwrap_or_default();
                Ok(vm.alloc_object(HashMap::from([
                    ("name".to_owned(), JsVal::Str("Error".to_owned())),
                    ("message".to_owned(), JsVal::Str(msg)),
                ])))
            }
            "XMLHttpRequest" => Ok(vm.alloc_object(HashMap::from([
                ("__wispXhr".to_owned(), JsVal::Bool(true)),
                ("readyState".to_owned(), JsVal::Number(0.0)),
                ("status".to_owned(), JsVal::Number(0.0)),
                ("responseText".to_owned(), JsVal::Str(String::new())),
            ]))),
            "URL" => {
                let raw = args.first().map(ToString::to_string).unwrap_or_default();
                let base = args.get(1).map(ToString::to_string);
                let parsed = match base {
                    Some(b) => url::Url::parse(&b)
                        .and_then(|u| u.join(&raw))
                        .map_err(|e| JsVal::Str(format!("URL inválida: {e}")))?,
                    None => url::Url::parse(&raw)
                        .map_err(|e| JsVal::Str(format!("URL inválida: {e}")))?,
                };
                Ok(JsVal::NativeHandle(format!("url:{parsed}")))
            }
            "TextEncoder" => Ok(JsVal::NativeHandle("TextEncoder".to_owned())),
            "TextDecoder" => Ok(JsVal::NativeHandle("TextDecoder".to_owned())),
            "MutationObserver" => {
                let Some(JsVal::Function(cb_fn_id)) = args.first() else {
                    return Err(JsVal::Str(
                        "MutationObserver requiere una función callback".to_owned(),
                    ));
                };
                let oid = self.observers.len();
                self.observers.push(MutationObserverState {
                    cb_fn_id: *cb_fn_id,
                    targets: Vec::new(),
                    subtree: false,
                    attributes: false,
                    child_list: true,
                    records: Vec::new(),
                });
                Ok(JsVal::NativeHandle(format!("observer:{oid}")))
            }
            "Event" | "CustomEvent" => {
                let ev_type = args.first().map(ToString::to_string).unwrap_or_default();
                let mut map = HashMap::from([
                    ("type".to_owned(), JsVal::Str(ev_type)),
                    ("bubbles".to_owned(), JsVal::Bool(false)),
                    ("cancelable".to_owned(), JsVal::Bool(false)),
                    ("detail".to_owned(), JsVal::Null),
                ]);
                if let Some(JsVal::Object(opt_id)) = args.get(1)
                    && let Some(opts) = vm.objects.get(*opt_id)
                {
                    if let Some(b) = opts.get("bubbles") {
                        map.insert("bubbles".to_owned(), b.clone());
                    }
                    if let Some(c) = opts.get("cancelable") {
                        map.insert("cancelable".to_owned(), c.clone());
                    }
                    if let Some(d) = opts.get("detail") {
                        map.insert("detail".to_owned(), d.clone());
                    }
                }
                Ok(vm.alloc_object(map))
            }
            _ => Err(JsVal::Str(format!("constructor no definido `{ctor}`"))),
        }
    }
}

pub(crate) fn run_scripts_native(
    document: &Html,
    classic: Vec<(String, String)>,
    modules: Vec<ModuleEntry>,
    module_scripts: &HashMap<String, String>,
    page_url: Option<String>,
    viewport_width: f32,
    policy: &wisp_net::NetworkPolicy,
    geometry_hook: Option<GeometryHook<'_>>,
) -> (Option<String>, Vec<String>) {
    let mut warnings = Vec::new();
    init_fetch_state(page_url.as_deref(), *policy);

    let mut host = NativeDomHost::from_html(
        document,
        page_url.as_deref(),
        viewport_width,
        *policy,
        geometry_hook,
    );
    let mut vm = VmRuntime::new();
    host.init_globals(&mut vm);

    for (label, source) in classic {
        if let Err(error) = vm.eval_script(&source, &mut host) {
            push_warning(
                &mut warnings,
                format!("Falló {label}: {}", truncate_error(&error)),
            );
        }
        host.drain_async_tasks(&mut vm);
    }

    host.dispatch_lifecycle_events(&mut vm);

    let mut visited_modules = HashSet::new();
    for module in modules {
        if let Err(error) = run_module_entry(
            &module,
            module_scripts,
            &mut visited_modules,
            &mut vm,
            &mut host,
        ) {
            push_warning(
                &mut warnings,
                format!(
                    "No se pudo analizar {}: {}",
                    module.label,
                    truncate_error(&error)
                ),
            );
        }
        host.drain_async_tasks(&mut vm);
    }

    drain_fetch_warnings(&mut warnings);
    for w in std::mem::take(&mut host.warnings) {
        push_warning(&mut warnings, w);
    }

    if host.oversized {
        push_warning(
            &mut warnings,
            "El DOM generado por JavaScript supera el límite de tamaño de Wisp.".to_owned(),
        );
        return (None, warnings);
    }

    let html = if host.mutated {
        let serialized = host.serialize_html();
        if serialized.len() > MAX_SERIALIZED_HTML_BYTES {
            push_warning(
                &mut warnings,
                "El DOM generado por JavaScript supera el límite de tamaño de Wisp.".to_owned(),
            );
            None
        } else {
            Some(serialized)
        }
    } else {
        None
    };

    (html, warnings)
}

pub(crate) fn evaluate_expression_native(
    document: &Html,
    page_url: Option<&str>,
    viewport_width: f32,
    source: &str,
    policy: wisp_net::NetworkPolicy,
    geometry_hook: Option<GeometryHook<'_>>,
) -> anyhow::Result<EvalResult> {
    let mut warnings = Vec::new();
    init_fetch_state(page_url, policy);

    let mut host = NativeDomHost::from_html(
        document,
        page_url,
        viewport_width,
        policy,
        geometry_hook,
    );
    let mut vm = VmRuntime::new();
    host.init_globals(&mut vm);

    let evaluated = vm
        .eval_script(source, &mut host)
        .map_err(|error| anyhow::anyhow!("falló la evaluación: {}", truncate_error(&error)))?;
    host.drain_async_tasks(&mut vm);

    let value = jsval_to_json(&evaluated, &vm);
    drain_fetch_warnings(&mut warnings);
    for w in std::mem::take(&mut host.warnings) {
        push_warning(&mut warnings, w);
    }

    let html = if host.oversized {
        push_warning(
            &mut warnings,
            "El DOM generado por JavaScript supera el límite de tamaño de Wisp.".to_owned(),
        );
        None
    } else if host.mutated {
        let serialized = host.serialize_html();
        if serialized.len() > MAX_SERIALIZED_HTML_BYTES {
            push_warning(
                &mut warnings,
                "El DOM generado por JavaScript supera el límite de tamaño de Wisp.".to_owned(),
            );
            None
        } else {
            Some(serialized)
        }
    } else {
        None
    };

    if warnings.len() > MAX_SCRIPT_WARNINGS {
        warnings.truncate(MAX_SCRIPT_WARNINGS);
    }

    Ok(EvalResult {
        value,
        html,
        warnings,
    })
}

fn run_module_entry(
    module: &ModuleEntry,
    module_scripts: &HashMap<String, String>,
    visited: &mut HashSet<String>,
    vm: &mut VmRuntime,
    host: &mut NativeDomHost,
) -> Result<(), String> {
    if !visited.insert(module.url.clone()) {
        return Ok(());
    }
    let prepared = preprocess_module(
        &module.source,
        &module.url,
        module_scripts,
        visited,
        vm,
        host,
    )?;
    let _ = vm.eval_script(&prepared, host)?;
    Ok(())
}

fn preprocess_module(
    src: &str,
    module_url: &str,
    module_scripts: &HashMap<String, String>,
    visited: &mut HashSet<String>,
    vm: &mut VmRuntime,
    host: &mut NativeDomHost,
) -> Result<String, String> {
    let mut out = String::new();
    for raw_line in src.lines() {
        let line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("import ")
            && let Some((_, spec_part)) = rest.split_once(" from ")
        {
            let spec = spec_part
                .trim()
                .trim_end_matches(';')
                .trim()
                .trim_matches(|c| c == '"' || c == '\'');
            let base = module_url.split('#').next().unwrap_or(module_url);
            let resolved = resolve_script_url(Some(base), spec)
                .ok_or_else(|| format!("no se pudo resolver el módulo `{spec}`"))?;
            let dep_src = module_scripts
                .get(&resolved)
                .ok_or_else(|| format!("módulo no encontrado `{resolved}`"))?;
            let dep_entry = ModuleEntry {
                label: resolved.clone(),
                url: resolved,
                source: dep_src.clone(),
            };
            run_module_entry(&dep_entry, module_scripts, visited, vm, host)?;
            continue;
        }
        let cleaned = raw_line
            .replace("import.meta.url", &format!("{module_url:?}"))
            .replace("export const ", "const ")
            .replace("export let ", "let ")
            .replace("export var ", "var ")
            .replace("export function ", "function ");
        out.push_str(&cleaned);
        out.push('\n');
    }
    Ok(out)
}

fn init_fetch_state(page_url: Option<&str>, policy: wisp_net::NetworkPolicy) {
    FETCH_STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.page_url = page_url.and_then(|url| url::Url::parse(url).ok());
        state.policy = policy;
        state.used = 0;
        state.deadline = Some(Instant::now() + MAX_FETCH_TOTAL_TIME);
        state.warnings.clear();
    });
}

fn drain_fetch_warnings(warnings: &mut Vec<String>) {
    let fetch_warnings = FETCH_STATE.with(|state| std::mem::take(&mut state.borrow_mut().warnings));
    for warning in fetch_warnings {
        push_warning(warnings, warning);
    }
}

fn jsval_to_json(val: &JsVal, vm: &VmRuntime) -> Option<Value> {
    match val {
        JsVal::Undefined => None,
        JsVal::Null => Some(Value::Null),
        JsVal::Bool(b) => Some(Value::Bool(*b)),
        JsVal::Number(n) => {
            if !n.is_finite() {
                Some(Value::Null)
            } else if n.fract() == 0.0 && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 {
                Some(Value::Number((*n as i64).into()))
            } else {
                serde_json::Number::from_f64(*n).map(Value::Number)
            }
        }
        JsVal::Str(s) => Some(Value::String(s.clone())),
        JsVal::Array(items) => Some(Value::Array(
            items
                .iter()
                .map(|i| jsval_to_json(i, vm).unwrap_or(Value::Null))
                .collect(),
        )),
        JsVal::Object(id) => {
            let map = vm.objects.get(*id)?;
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                if let Some(jv) = jsval_to_json(v, vm) {
                    obj.insert(k.clone(), jv);
                }
            }
            Some(Value::Object(obj))
        }
        JsVal::DomNode(id) => Some(Value::String(format!("[DomNode #{id}]"))),
        JsVal::Function(_) | JsVal::NativeHandle(_) => None,
    }
}

fn json_to_jsval(val: &Value, vm: &mut VmRuntime) -> JsVal {
    match val {
        Value::Null => JsVal::Null,
        Value::Bool(b) => JsVal::Bool(*b),
        Value::Number(n) => JsVal::Number(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => JsVal::Str(s.clone()),
        Value::Array(arr) => JsVal::Array(arr.iter().map(|i| json_to_jsval(i, vm)).collect()),
        Value::Object(map) => {
            let mut out = HashMap::new();
            for (k, v) in map {
                out.insert(k.clone(), json_to_jsval(v, vm));
            }
            vm.alloc_object(out)
        }
    }
}

fn location_part(url: Option<&str>, prop: &str) -> String {
    let Some(parsed) = url.and_then(|u| url::Url::parse(u).ok()) else {
        return match prop {
            "href" => "about:blank".to_owned(),
            "origin" => "null".to_owned(),
            "protocol" => "about:".to_owned(),
            _ => String::new(),
        };
    };
    let host_only = parsed.host_str().unwrap_or_default();
    let host_with_port = match parsed.port() {
        Some(p) => format!("{host_only}:{p}"),
        None => host_only.to_owned(),
    };
    let origin = if host_with_port.is_empty() {
        "null".to_owned()
    } else {
        format!("{}://{host_with_port}", parsed.scheme())
    };
    match prop {
        "href" => parsed.to_string(),
        "origin" => origin,
        "protocol" => format!("{}:", parsed.scheme()),
        "host" => host_with_port,
        "hostname" => host_only.to_owned(),
        "port" => parsed.port().map(|p| p.to_string()).unwrap_or_default(),
        "pathname" => parsed.path().to_owned(),
        "search" => parsed.query().map(|q| format!("?{q}")).unwrap_or_default(),
        "hash" => parsed
            .fragment()
            .map(|f| format!("#{f}"))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn parse_inline_styles(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for decl in raw.split(';') {
        if let Some((k, v)) = decl.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_owned();
            if !key.is_empty() {
                out.push((key, val));
            }
        }
    }
    out
}

fn camel_to_kebab(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        if ch.is_ascii_uppercase() {
            out.push('-');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

pub(crate) fn native_idl_supports_interface(iface: &str) -> bool {
    matches!(
        iface,
        "EventTarget" | "Node" | "Element" | "HTMLElement" | "Document" | "Window"
    )
}

pub(crate) fn native_idl_supports_member(iface: &str, member: &str) -> bool {
    match iface {
        "EventTarget" => matches!(
            member,
            "addEventListener" | "removeEventListener" | "dispatchEvent"
        ),
        "Node" => matches!(
            member,
            "nodeType"
                | "nodeName"
                | "nodeValue"
                | "parentNode"
                | "parentElement"
                | "childNodes"
                | "firstChild"
                | "lastChild"
                | "previousSibling"
                | "nextSibling"
                | "appendChild"
                | "insertBefore"
                | "removeChild"
                | "replaceChild"
                | "cloneNode"
                | "hasChildNodes"
                | "contains"
                | "isEqualNode"
        ),
        "Element" => matches!(
            member,
            "tagName"
                | "id"
                | "className"
                | "classList"
                | "attributes"
                | "innerHTML"
                | "outerHTML"
                | "getAttribute"
                | "setAttribute"
                | "removeAttribute"
                | "hasAttribute"
                | "toggleAttribute"
                | "querySelector"
                | "querySelectorAll"
                | "closest"
                | "insertAdjacentHTML"
                | "getBoundingClientRect"
                | "clientWidth"
                | "clientHeight"
                | "scrollWidth"
                | "scrollHeight"
        ),
        "HTMLElement" => matches!(
            member,
            "title"
                | "lang"
                | "hidden"
                | "dir"
                | "innerText"
                | "outerText"
                | "accessKey"
                | "click"
                | "focus"
                | "blur"
                | "offsetWidth"
                | "offsetHeight"
                | "offsetTop"
                | "offsetLeft"
        ),
        "Document" => matches!(
            member,
            "doctype"
                | "documentElement"
                | "title"
                | "URL"
                | "cookie"
                | "readyState"
                | "images"
                | "links"
                | "location"
                | "getElementById"
                | "getElementsByTagName"
                | "getElementsByClassName"
                | "querySelector"
                | "querySelectorAll"
                | "elementFromPoint"
                | "createElement"
                | "createTextNode"
                | "createDocumentFragment"
                | "open"
                | "close"
                | "write"
        ),
        "Window" => matches!(
            member,
            "window"
                | "self"
                | "document"
                | "location"
                | "navigator"
                | "performance"
                | "crypto"
                | "innerWidth"
                | "innerHeight"
                | "devicePixelRatio"
                | "scrollX"
                | "scrollY"
                | "scrollTo"
                | "getComputedStyle"
                | "closed"
                | "name"
                | "event"
                | "alert"
                | "close"
                | "focus"
                | "blur"
                | "open"
        ),
        _ => false,
    }
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

fn is_void_tag(tag: &str) -> bool {
    matches!(
        tag.to_ascii_lowercase().as_str(),
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

#[derive(Clone, Copy)]
enum Combinator {
    Descendant,
    Child,
}

fn parse_selector_chain(sel: &str) -> Vec<(Combinator, String)> {
    let mut out = Vec::new();
    let mut next_comb = Combinator::Descendant;
    for part in sel.split_whitespace() {
        if part == ">" {
            next_comb = Combinator::Child;
        } else {
            out.push((next_comb, part.to_owned()));
            next_comb = Combinator::Descendant;
        }
    }
    out
}

fn parse_px_val(raw: &str) -> Option<f32> {
    let trimmed = raw.trim();
    if let Some(num) = trimmed.strip_suffix("px") {
        num.trim().parse::<f32>().ok()
    } else {
        trimmed.parse::<f32>().ok()
    }
}

fn fill_random_bytes(out: &mut [u8]) {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    static SEQ: AtomicU64 = AtomicU64::new(1);
    let rs = RandomState::new();
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut chunk_idx = 0usize;
    while chunk_idx < out.len() {
        let mut h = rs.build_hasher();
        h.write_u64(nanos);
        h.write_u64(SEQ.fetch_add(1, Ordering::Relaxed));
        h.write_usize(chunk_idx);
        let word = h.finish().to_le_bytes();
        for b in word {
            if chunk_idx >= out.len() {
                break;
            }
            out[chunk_idx] = b;
            chunk_idx += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_computed_style_and_element_from_point() {
        let html = r#"<html><body><div id="box" style="width: 240px; height: 80px; color: red;">Hola</div></body></html>"#;
        let res = crate::evaluate_expression(
            html,
            Some("https://example.com/"),
            1280.0,
            r#"(() => {
                const el = document.getElementById("box");
                const r = el.getBoundingClientRect();
                const cs = window.getComputedStyle(el);
                const hit = document.elementFromPoint(10, 10);
                return [r.width, r.height, el.offsetWidth, cs.color, hit && hit.id];
            })()"#,
        )
        .expect("eval ok");
        assert_eq!(res.value, Some(serde_json::json!([240.0, 80.0, 240.0, "red", "box"])));
    }

    #[test]
    fn mutation_observer_custom_event_performance_and_crypto() {
        let html = r#"<html><body><div id="root"></div></body></html>"#;
        let res = crate::evaluate_expression(
            html,
            Some("https://example.com/"),
            1280.0,
            r#"(() => {
                const root = document.getElementById("root");
                let observed = 0;
                const obs = new MutationObserver((recs) => { observed += recs.length; });
                obs.observe(root, { childList: true, attributes: true });
                root.setAttribute("data-ok", "1");
                const recs = obs.takeRecords();
                let gotDetail = null;
                root.addEventListener("ping", (e) => { gotDetail = e.detail.n; });
                root.dispatchEvent(new CustomEvent("ping", { detail: { n: 42 }, bubbles: true }));
                const uuid = crypto.randomUUID();
                const arr = crypto.getRandomValues(new Uint8Array(4));
                const t = performance.now();
                return [recs.length, gotDetail, uuid.length, arr.length, t >= 0];
            })()"#,
        )
        .expect("eval ok");
        assert_eq!(res.value, Some(serde_json::json!([1, 42, 36, 4, true])));
    }

    #[test]
    fn stealth_navigator_and_document_cookie() {
        let policy = wisp_net::NetworkPolicy {
            stealth: true,
            ..Default::default()
        };
        let res = crate::evaluate_expression_with_policy(
            "<html><body></body></html>",
            Some("https://example.com/"),
            1280.0,
            r#"(() => {
                document.cookie = "sid=abc123; Path=/";
                return [navigator.webdriver, navigator.userAgent.includes("Chrome/"), !!window.chrome, document.cookie];
            })()"#,
            &policy,
        )
        .expect("eval ok");
        assert_eq!(res.value, Some(serde_json::json!([false, true, true, "sid=abc123"])));
    }
}
