// Wisp's JavaScript DOM.
//
// This file is loaded into the Boa context before any page script. It builds a
// mutable tree from the JSON produced by Rust, exposes a deliberately small
// browser-like API, and serializes the result back to HTML so Rust can run the
// normal CSS/layout pipeline on the mutated document.
(function () {
    "use strict";

    const MAX_NODES = 20000;
    const MAX_TIMER_CALLBACKS = 100;
    const MAX_INTERVAL_REPEATS = 9;
    const MAX_ANIMATION_FRAMES = 10;
    const MAX_RUNTIME_WARNINGS = 8;
    const MAX_SNAPSHOT_BYTES = 8 * 1024 * 1024;
    const DEFAULT_VIEWPORT_HEIGHT = 800;
    const VOID_TAGS = new Set([
        "area", "base", "br", "col", "embed", "hr", "img", "input",
        "link", "meta", "param", "source", "track", "wbr"
    ]);
    const RAW_TEXT_TAGS = new Set(["script", "style", "xmp", "iframe", "noembed", "noframes"]);
    const BLOCK_TAGS = new Set([
        "address", "article", "aside", "blockquote", "body", "dd", "div", "dl",
        "dt", "fieldset", "figcaption", "figure", "footer", "form", "h1", "h2",
        "h3", "h4", "h5", "h6", "header", "hr", "html", "li", "main", "nav",
        "ol", "p", "pre", "section", "table", "tbody", "td", "tfoot", "th",
        "thead", "tr", "ul"
    ]);

    const viewport = {
        width: Math.max(1, Number(__wispViewport.width) || 1),
        height: DEFAULT_VIEWPORT_HEIGHT
    };
    const pageUrl = __wispUrl || {};
    const runtimeWarnings = [];
    let nodeCount = 0;
    let mutationCount = 0;
    let nextUid = 1;
    let documentObject = null;
    let windowTarget = null;
    let timerSequence = 0;
    const timers = new Map();
    const animationFrames = new Map();
    // Web Components básicos: definición por nombre y ciclo de vida conectado.
    const customDefinitions = new Map();
    const customDefinitionWaiters = new Map();

    function reportError(error) {
        if (runtimeWarnings.length >= MAX_RUNTIME_WARNINGS) return;
        let message;
        try {
            message = error && error.message ? String(error.message) : String(error);
        } catch (_) {
            message = "error desconocido";
        }
        runtimeWarnings.push(message);
    }

    function bump() {
        mutationCount++;
    }

    function cssName(property) {
        if (property === "cssFloat") return "float";
        if (property.startsWith("--")) return property;
        return String(property).replace(/[A-Z]/g, character => "-" + character.toLowerCase());
    }

    function parseStyle(text) {
        const declarations = new Map();
        for (const part of String(text || "").split(";")) {
            const separator = part.indexOf(":");
            if (separator < 0) continue;
            const name = part.slice(0, separator).trim();
            const value = part.slice(separator + 1).trim();
            if (name) declarations.set(name.startsWith("--") ? name : cssName(name), value);
        }
        return declarations;
    }

    function styleText(element) {
        let text = "";
        for (const [name, value] of element._style) {
            if (value === null || value === undefined || value === "") continue;
            text += name + ": " + value + ";";
        }
        return text;
    }

    function escapeText(text) {
        return String(text)
            .replace(/&/g, "&amp;")
            .replace(/</g, "&lt;")
            .replace(/>/g, "&gt;");
    }

    function escapeAttribute(text) {
        return escapeText(text).replace(/"/g, "&quot;");
    }

    function decodeEntities(text) {
        return String(text)
            .replace(/&nbsp;/gi, "\u00a0")
            .replace(/&lt;/gi, "<")
            .replace(/&gt;/gi, ">")
            .replace(/&quot;/gi, '"')
            .replace(/&#39;|&apos;/gi, "'")
            .replace(/&#x([0-9a-f]+);/gi, (_, hex) => String.fromCodePoint(parseInt(hex, 16)))
            .replace(/&#([0-9]+);/g, (_, decimal) => String.fromCodePoint(parseInt(decimal, 10)))
            .replace(/&amp;/gi, "&");
    }

    function matchSimple(element, selector) {
        const simple = selector.trim();
        if (!simple || /[\s>+~:[\]]/.test(simple)) return false;
        const id = simple.match(/#([\w-]+)/);
        const tag = simple.match(/^[a-zA-Z][\w-]*/);
        const classes = [];
        const classPattern = /\.([\w-]+)/g;
        let match;
        while ((match = classPattern.exec(simple))) classes.push(match[1]);
        const consumed = simple.replace(/#[\w-]+|\.([\w-]+)/g, "");
        if (consumed.includes("#") || consumed.includes(".")) return false;
        if (id && element.id !== id[1]) return false;
        if (tag && element.localName !== tag[0].toLowerCase()) return false;
        const ownClasses = element.className.split(/\s+/).filter(Boolean);
        for (const name of classes) {
            if (!ownClasses.includes(name)) return false;
        }
        return true;
    }

    function matches(element, selector) {
        return String(selector).split(",").some(part => matchSimple(element, part));
    }

    function walkElements(root, callback) {
        for (const child of root._children) {
            if (child._type === 1) {
                callback(child);
                walkElements(child, callback);
            }
        }
    }

    function queryAll(root, selector) {
        const result = [];
        walkElements(root, element => {
            if (matches(element, selector)) result.push(element);
        });
        return result;
    }

    // ---------------------------------------------------- custom elements
    // Wisp no puede reemplazar el objeto que ya ocupa la posición del nodo, así
    // que «sube de nivel» el elemento existente: adopta el prototipo de la
    // clase definida y dispara el ciclo de vida. Al estar la sombra degradada
    // a los hijos del propio elemento, el contenido de los componentes se
    // serializa y se pinta con el pipeline normal de Wisp.
    function attributeChanged(element, name, oldValue, newValue) {
        if (!element._definition) return;
        if (!element._definition.observed.includes(String(name).toLowerCase())) return;
        const callback = element.attributeChangedCallback;
        if (typeof callback !== "function") return;
        try {
            callback.call(element, String(name).toLowerCase(), oldValue, newValue);
        } catch (error) {
            reportError(error);
        }
    }

    function elementConnected(element) {
        if (!element._definition || element._connected) return;
        element._connected = true;
        const callback = element.connectedCallback;
        if (typeof callback !== "function") return;
        try {
            callback.call(element);
        } catch (error) {
            reportError(error);
        }
    }

    function elementDisconnected(element) {
        if (!element._definition || !element._connected) return;
        element._connected = false;
        const callback = element.disconnectedCallback;
        if (typeof callback !== "function") return;
        try {
            callback.call(element);
        } catch (error) {
            reportError(error);
        }
    }

    function upgradeCustomElement(element) {
        if (!element || element._type !== 1 || element._definition) return;
        const definition = customDefinitions.get(element._tag);
        if (!definition) return;
        element._definition = definition;
        try {
            Object.setPrototypeOf(element, definition.constructor.prototype);
        } catch (error) {
            reportError(error);
        }
        // `attributeChangedCallback` para los atributos que ya tenía el HTML.
        for (const name of definition.observed) {
            if (element.hasAttribute(name)) {
                attributeChanged(element, name, null, element.getAttribute(name));
            }
        }
        if (element.isConnected) elementConnected(element);
    }

    function connectTree(node) {
        if (!node) return;
        if (node._type === 1) {
            upgradeCustomElement(node);
            elementConnected(node);
        }
        for (const child of node._children) connectTree(child);
    }

    function disconnectTree(node) {
        if (!node) return;
        if (node._type === 1) elementDisconnected(node);
        for (const child of node._children) disconnectTree(child);
    }

    class WispEvent {
        constructor(type, options) {
            this.type = String(type);
            this.bubbles = !!(options && options.bubbles);
            this.cancelable = !!(options && options.cancelable);
            this.defaultPrevented = false;
            this.target = null;
            this.currentTarget = null;
            this._stopped = false;
            this._immediateStopped = false;
        }

        preventDefault() {
            if (this.cancelable) this.defaultPrevented = true;
        }

        stopPropagation() {
            this._stopped = true;
        }

        stopImmediatePropagation() {
            this._stopped = true;
            this._immediateStopped = true;
        }
    }

    class WispCustomEvent extends WispEvent {
        constructor(type, options) {
            super(type, options);
            this.detail = options && options.detail !== undefined ? options.detail : null;
        }
    }

    class WispMouseEvent extends WispEvent {
        constructor(type, options) {
            super(type, options);
            this.clientX = Number(options && options.clientX) || 0;
            this.clientY = Number(options && options.clientY) || 0;
            this.button = Number(options && options.button) || 0;
        }
    }

    class WispKeyboardEvent extends WispEvent {
        constructor(type, options) {
            super(type, options);
            this.key = String((options && options.key) || "");
            this.code = String((options && options.code) || "");
        }
    }

    function invokeListeners(target, event) {
        if (!target._listeners) return;
        const listeners = target._listeners.get(event.type);
        if (!listeners || listeners.length === 0) return;
        for (const listener of listeners.slice()) {
            if (event._immediateStopped) break;
            if (listener.once) target.removeEventListener(event.type, listener.callback, listener.capture);
            try {
                if (typeof listener.callback === "function") {
                    listener.callback.call(target, event);
                } else if (listener.callback && typeof listener.callback.handleEvent === "function") {
                    listener.callback.handleEvent(event);
                }
            } catch (error) {
                reportError(error);
            }
        }
    }

    function dispatchOn(target, event) {
        event.target = target;
        let node = target;
        while (node) {
            event.currentTarget = node;
            invokeListeners(node, event);
            if (event._stopped || !event.bubbles) break;
            if (node === documentObject) {
                invokeListeners(windowTarget, event);
                break;
            }
            node = node._parent;
        }
        event.currentTarget = null;
        return !event.defaultPrevented;
    }

    class WispEventTarget {
        constructor() {
            this._listeners = null;
        }

        addEventListener(type, callback, options) {
            if (!callback) return;
            const capture = typeof options === "boolean" ? options : !!(options && options.capture);
            const once = !!(options && options.once);
            if (!this._listeners) this._listeners = new Map();
            let listeners = this._listeners.get(String(type));
            if (!listeners) {
                listeners = [];
                this._listeners.set(String(type), listeners);
            }
            if (!listeners.some(entry => entry.callback === callback && entry.capture === capture)) {
                listeners.push({ callback, capture, once });
            }
        }

        removeEventListener(type, callback, options) {
            if (!this._listeners) return;
            const capture = typeof options === "boolean" ? options : !!(options && options.capture);
            const listeners = this._listeners.get(String(type));
            if (!listeners) return;
            const index = listeners.findIndex(entry => entry.callback === callback && entry.capture === capture);
            if (index >= 0) listeners.splice(index, 1);
        }

        dispatchEvent(event) {
            if (!event || typeof event.type !== "string") {
                throw new TypeError("dispatchEvent requiere un Event");
            }
            event.target = this;
            event.currentTarget = this;
            invokeListeners(this, event);
            event.currentTarget = null;
            return !event.defaultPrevented;
        }
    }

    // Compara dos nodos como lo hace `Node.isEqualNode`: mismo tipo, mismos
    // datos y, recursivamente, mismos hijos y atributos.
    function nodesEqual(a, b) {
        if (a === b) return true;
        if (!a || !b || a._type !== b._type) return false;
        if (a._type === 3 || a._type === 8) return a._text === b._text;
        if (a._type === 1) {
            if (a._tag !== b._tag) return false;
            if (a._attributes.size !== b._attributes.size) return false;
            for (const [name, value] of a._attributes) {
                if (b._attributes.get(name) !== value) return false;
            }
        }
        if (a._children.length !== b._children.length) return false;
        for (let index = 0; index < a._children.length; index++) {
            if (!nodesEqual(a._children[index], b._children[index])) return false;
        }
        return true;
    }

    class WispNode extends WispEventTarget {
        constructor(nodeType) {
            super();
            nodeCount++;
            if (nodeCount > MAX_NODES) {
                throw new RangeError("Wisp: se alcanzó el límite de nodos del DOM");
            }
            this._uid = nextUid++;
            this._type = nodeType;
            this._parent = null;
            this._children = [];
        }

        get nodeType() { return this._type; }

        get nodeName() {
            switch (this._type) {
                case 1: return this._tag.toUpperCase();
                case 3: return "#text";
                case 8: return "#comment";
                case 9: return "#document";
                case 11: return "#document-fragment";
                default: return "#node";
            }
        }

        get nodeValue() { return this._text === undefined ? null : this._text; }
        set nodeValue(value) {
            if (this._text === undefined) return;
            this._text = value === null || value === undefined ? "" : String(value);
            bump();
        }

        get parentNode() { return this._parent; }
        get parentElement() { return this._parent && this._parent._type === 1 ? this._parent : null; }
        get childNodes() { return this._children.slice(); }
        get firstChild() { return this._children[0] || null; }
        get lastChild() { return this._children[this._children.length - 1] || null; }
        get ownerDocument() { return documentObject; }
        get isConnected() {
            let node = this;
            while (node._parent) node = node._parent;
            return node === documentObject;
        }

        get nextSibling() {
            if (!this._parent) return null;
            const index = this._parent._children.indexOf(this);
            return index >= 0 ? this._parent._children[index + 1] || null : null;
        }

        get previousSibling() {
            if (!this._parent) return null;
            const index = this._parent._children.indexOf(this);
            return index > 0 ? this._parent._children[index - 1] : null;
        }

        appendChild(child) {
            return this.insertBefore(child, null);
        }

        insertBefore(child, reference) {
            if (!child || typeof child._type !== "number") {
                throw new TypeError("insertBefore requiere un nodo");
            }
            if (child._type === 9) throw new Error("no se puede insertar un documento");
            if (reference === child) return child;
            if (reference && reference._parent !== this) {
                throw new Error("el nodo de referencia no es hijo");
            }
            for (let ancestor = this; ancestor; ancestor = ancestor._parent) {
                if (ancestor === child) {
                    throw new Error("no se puede insertar un ancestro dentro de su descendiente");
                }
            }
            if (child._parent) child._parent.removeChild(child);
            if (child._type === 11) {
                const nodes = child._children.slice();
                child._children.length = 0;
                for (const node of nodes) {
                    node._parent = null;
                    this.insertBefore(node, reference);
                }
                return child;
            }
            const index = reference ? this._children.indexOf(reference) : this._children.length;
            this._children.splice(index < 0 ? this._children.length : index, 0, child);
            child._parent = this;
            bump();
            // Insertar en el documento conecta los componentes personalizados
            // de todo el subárbol (incluidos los creados antes de definirse).
            if (this.isConnected) connectTree(child);
            return child;
        }

        removeChild(child) {
            const index = this._children.indexOf(child);
            if (index < 0) throw new Error("el nodo no es hijo");
            const wasConnected = child.isConnected;
            this._children.splice(index, 1);
            child._parent = null;
            bump();
            if (wasConnected) disconnectTree(child);
            return child;
        }

        replaceChild(newChild, oldChild) {
            const index = this._children.indexOf(oldChild);
            if (index < 0) throw new Error("el nodo no es hijo");
            if (newChild === oldChild) return oldChild;
            for (let ancestor = this; ancestor; ancestor = ancestor._parent) {
                if (ancestor === newChild) {
                    throw new Error("no se puede insertar un ancestro dentro de su descendiente");
                }
            }
            if (newChild._parent) newChild._parent.removeChild(newChild);
            oldChild._parent = null;
            this._children[index] = newChild;
            newChild._parent = this;
            bump();
            return oldChild;
        }

        hasChildNodes() {
            return this._children.length > 0;
        }

        contains(node) {
            if (!node) return false;
            let current = node;
            while (current) {
                if (current === this) return true;
                current = current._parent;
            }
            return false;
        }

        isEqualNode(other) { return nodesEqual(this, other); }

        cloneNode(deep) {
            if (this._type === 3) return new WispText(this._text);
            if (this._type === 8) return new WispComment(this._text);
            const clone = this._type === 11 ? new WispDocumentFragment() : new WispElement(this._tag);
            if (this._type === 1) {
                for (const [name, value] of this._attributes) clone._attributes.set(name, value);
                for (const [name, value] of this._style) clone._style.set(name, value);
                clone._hasStyle = this._hasStyle;
                clone._classNameIsSvg = this._classNameIsSvg;
            }
            if (deep) {
                for (const child of this._children) {
                    const childClone = child.cloneNode(true);
                    childClone._parent = clone;
                    clone._children.push(childClone);
                }
            }
            return clone;
        }

        dispatchEvent(event) {
            if (!event || typeof event.type !== "string") {
                throw new TypeError("dispatchEvent requiere un Event");
            }
            return dispatchOn(this, event);
        }

        get textContent() {
            if (this._type === 3 || this._type === 8) return this._text;
            let text = "";
            for (const child of this._children) {
                if (child._type === 3) text += child._text;
                else if (child._type === 1) text += child.textContent;
            }
            return text;
        }

        set textContent(value) {
            const text = value === null || value === undefined ? "" : String(value);
            for (const child of this._children) child._parent = null;
            this._children.length = 0;
            if (text !== "") {
                const node = new WispText(text);
                node._parent = this;
                this._children.push(node);
            }
            bump();
        }

        get innerText() { return this.textContent; }
        set innerText(value) { this.textContent = value; }
    }

    class WispText extends WispNode {
        constructor(text) {
            super(3);
            this._text = String(text);
        }

        get nodeName() { return "#text"; }
        get data() { return this._text; }
        set data(value) { this._text = String(value); bump(); }
        get wholeText() { return this._text; }
    }

    class WispComment extends WispNode {
        constructor(text) {
            super(8);
            this._text = String(text);
        }

        get nodeName() { return "#comment"; }
        get data() { return this._text; }
        set data(value) { this._text = String(value); bump(); }
    }

    class WispDocumentFragment extends WispNode {
        constructor() {
            super(11);
        }

        get nodeName() { return "#document-fragment"; }
    }

    class WispElement extends WispNode {
        constructor(tag) {
            super(1);
            this._tag = String(tag || "div").toLowerCase();
            this._attributes = new Map();
            this._style = new Map();
            this._hasStyle = false;
            this._classNameIsSvg = false;
            this._styleProxy = null;
            this._classList = null;
            this._datasetProxy = null;
        }

        get tagName() { return this._tag.toUpperCase(); }
        get nodeName() { return this._tag.toUpperCase(); }
        get localName() { return this._tag; }
        get namespaceURI() { return "http://www.w3.org/1999/xhtml"; }

        get id() { return this.getAttribute("id") || ""; }
        set id(value) { this.setAttribute("id", value); }

        get className() { return this.getAttribute("class") || ""; }
        set className(value) { this.setAttribute("class", value); }

        get title() { return this.getAttribute("title") || ""; }
        set title(value) { this.setAttribute("title", value === null || value === undefined ? "" : String(value)); }

        get lang() { return this.getAttribute("lang") || ""; }
        set lang(value) { this.setAttribute("lang", value === null || value === undefined ? "" : String(value)); }

        get dir() { return this.getAttribute("dir") || ""; }
        set dir(value) { this.setAttribute("dir", value === null || value === undefined ? "" : String(value)); }

        get accessKey() { return this.getAttribute("accesskey") || ""; }
        set accessKey(value) { this.setAttribute("accesskey", value === null || value === undefined ? "" : String(value)); }

        get hidden() { return this.hasAttribute("hidden"); }
        set hidden(value) {
            if (value) this.setAttribute("hidden", "");
            else this.removeAttribute("hidden");
        }

        get outerText() { return this.textContent; }
        set outerText(value) {
            const text = value === null || value === undefined ? "" : String(value);
            const parent = this._parent;
            if (!parent) {
                this.textContent = text;
                return;
            }
            parent.replaceChild(new WispText(text), this);
        }

        getAttribute(name) {
            const key = String(name).toLowerCase();
            if (key === "style") return this._hasStyle ? styleText(this) : null;
            const value = this._attributes.get(key);
            return value === undefined ? null : value;
        }

        setAttribute(name, value) {
            const key = String(name).toLowerCase();
            const text = String(value);
            const previous = this.getAttribute(key);
            if (key === "style") {
                this._style = parseStyle(text);
                this._hasStyle = true;
            } else {
                this._attributes.set(key, text);
            }
            bump();
            attributeChanged(this, key, previous, text);
        }

        removeAttribute(name) {
            const key = String(name).toLowerCase();
            const previous = this.getAttribute(key);
            if (key === "style") {
                this._style.clear();
                this._hasStyle = false;
            } else {
                this._attributes.delete(key);
            }
            bump();
            attributeChanged(this, key, previous, null);
        }

        hasAttribute(name) {
            const key = String(name).toLowerCase();
            if (key === "style") return this._hasStyle;
            return this._attributes.has(key);
        }

        toggleAttribute(name, force) {
            const present = this.hasAttribute(name);
            const next = force === undefined ? !present : !!force;
            if (next && !present) this.setAttribute(name, "");
            else if (!next && present) this.removeAttribute(name);
            return next;
        }

        get attributes() {
            const result = [];
            for (const [name, value] of this._attributes) result.push({ name, value });
            if (this._hasStyle) result.push({ name: "style", value: styleText(this) });
            return result;
        }

        get classList() {
            if (!this._classList) this._classList = new WispTokenList(this, "class");
            return this._classList;
        }

        get style() {
            if (!this._styleProxy) {
                const element = this;
                this._styleProxy = new Proxy({}, {
                    get(_target, property) {
                        if (property === "setProperty") {
                            return function (name, value) {
                                element._style.set(name.startsWith("--") ? name : cssName(name), String(value));
                                element._hasStyle = true;
                                bump();
                            };
                        }
                        if (property === "getPropertyValue") {
                            return function (name) {
                                return element._style.get(name.startsWith("--") ? name : cssName(name)) || "";
                            };
                        }
                        if (property === "removeProperty") {
                            return function (name) {
                                const key = name.startsWith("--") ? name : cssName(name);
                                const previous = element._style.get(key) || "";
                                element._style.delete(key);
                                bump();
                                return previous;
                            };
                        }
                        if (property === "cssText") return styleText(element);
                        if (property === "length") return element._style.size;
                        if (property === "item") {
                            return function (index) {
                                return Array.from(element._style.keys())[index] || "";
                            };
                        }
                        if (typeof property !== "string") return undefined;
                        return element._style.get(cssName(property)) || "";
                    },
                    set(_target, property, value) {
                        if (typeof property !== "string") return false;
                        const key = cssName(property);
                        const text = value === null || value === undefined ? "" : String(value);
                        if (text === "") element._style.delete(key);
                        else element._style.set(key, text);
                        element._hasStyle = true;
                        bump();
                        return true;
                    },
                    has(_target, property) {
                        return typeof property === "string" && element._style.has(cssName(property));
                    },
                    deleteProperty(_target, property) {
                        if (typeof property === "string") element._style.delete(cssName(property));
                        bump();
                        return true;
                    },
                    ownKeys() {
                        return Array.from(element._style.keys());
                    },
                    getOwnPropertyDescriptor(_target, property) {
                        if (typeof property !== "string") return undefined;
                        const value = element._style.get(cssName(property));
                        if (value === undefined) return undefined;
                        return {
                            value,
                            writable: true,
                            enumerable: true,
                            configurable: true
                        };
                    }
                });
            }
            return this._styleProxy;
        }

        set style(value) {
            this.setAttribute("style", value);
        }

        get dataset() {
            if (!this._datasetProxy) {
                const element = this;
                this._datasetProxy = new Proxy({}, {
                    get(_target, property) {
                        if (typeof property !== "string") return undefined;
                        const value = element.getAttribute("data-" + cssName(property));
                        return value === null ? undefined : value;
                    },
                    set(_target, property, value) {
                        if (typeof property !== "string") return false;
                        element.setAttribute("data-" + cssName(property), String(value));
                        return true;
                    },
                    deleteProperty(_target, property) {
                        if (typeof property !== "string") return false;
                        element.removeAttribute("data-" + cssName(property));
                        return true;
                    },
                    has(_target, property) {
                        return typeof property === "string"
                            && element.hasAttribute("data-" + cssName(property));
                    },
                    ownKeys() {
                        const keys = [];
                        for (const name of element._attributes.keys()) {
                            if (name.startsWith("data-")) {
                                keys.push(name.slice(5).replace(/-([a-z])/g, (_, character) => character.toUpperCase()));
                            }
                        }
                        return keys;
                    },
                    getOwnPropertyDescriptor(_target, property) {
                        if (typeof property !== "string") return undefined;
                        const value = element.getAttribute("data-" + cssName(property));
                        if (value === null) return undefined;
                        return {
                            value,
                            writable: true,
                            enumerable: true,
                            configurable: true
                        };
                    }
                });
            }
            return this._datasetProxy;
        }

        get children() {
            const result = [];
            for (const child of this._children) {
                if (child._type === 1) result.push(child);
            }
            return result;
        }

        get firstElementChild() {
            for (const child of this._children) if (child._type === 1) return child;
            return null;
        }

        get lastElementChild() {
            for (let index = this._children.length - 1; index >= 0; index--) {
                if (this._children[index]._type === 1) return this._children[index];
            }
            return null;
        }

        get childElementCount() {
            return this.children.length;
        }

        get parentElement() {
            return this._parent && this._parent._type === 1 ? this._parent : null;
        }

        get innerHTML() {
            let html = "";
            for (const child of this._children) html += serializeHtml(child);
            return html;
        }

        set innerHTML(value) {
            for (const child of this._children) child._parent = null;
            this._children.length = 0;
            const fragment = parseHtmlFragment(value === null || value === undefined ? "" : String(value));
            for (const child of fragment._children) {
                child._parent = this;
                this._children.push(child);
                if (this.isConnected) connectTree(child);
            }
            fragment._children.length = 0;
            bump();
        }

        get outerHTML() {
            return serializeHtml(this);
        }

        set outerHTML(value) {
            const parent = this._parent;
            if (!parent) return;
            const fragment = parseHtmlFragment(String(value));
            const index = parent._children.indexOf(this);
            if (index < 0) return;
            parent._children.splice(index, 1);
            this._parent = null;
            let offset = 0;
            for (const child of fragment._children.slice()) {
                child._parent = parent;
                parent._children.splice(index + offset, 0, child);
                offset++;
            }
            fragment._children.length = 0;
            bump();
        }

        querySelector(selector) {
            return queryAll(this, selector)[0] || null;
        }

        querySelectorAll(selector) {
            return queryAll(this, selector);
        }

        getElementsByTagName(tagName) {
            const tag = String(tagName).toLowerCase();
            const result = [];
            walkElements(this, element => {
                if (tag === "*" || element.localName === tag) result.push(element);
            });
            return result;
        }

        getElementsByClassName(className) {
            const classes = String(className).split(/\s+/).filter(Boolean);
            if (classes.length === 0) return [];
            const result = [];
            walkElements(this, element => {
                const own = element.className.split(/\s+/).filter(Boolean);
                if (classes.every(name => own.includes(name))) result.push(element);
            });
            return result;
        }

        matches(selector) {
            return matches(this, selector);
        }

        closest(selector) {
            let node = this;
            while (node && node._type === 1) {
                if (matches(node, selector)) return node;
                node = node._parent;
            }
            return null;
        }

        append(...nodes) {
            for (const node of nodes) {
                if (typeof node === "string") this.appendChild(new WispText(node));
                else this.appendChild(node);
            }
        }

        prepend(...nodes) {
            const first = this.firstChild;
            for (let index = nodes.length - 1; index >= 0; index--) {
                const node = nodes[index];
                const child = typeof node === "string" ? new WispText(node) : node;
                this.insertBefore(child, first);
            }
        }

        before(...nodes) {
            if (!this._parent) return;
            for (const node of nodes) {
                const sibling = typeof node === "string" ? new WispText(node) : node;
                this._parent.insertBefore(sibling, this);
            }
        }

        after(...nodes) {
            if (!this._parent) return;
            const next = this.nextSibling;
            for (const node of nodes) {
                const sibling = typeof node === "string" ? new WispText(node) : node;
                this._parent.insertBefore(sibling, next);
            }
        }

        replaceWith(...nodes) {
            if (!this._parent) return;
            const parent = this._parent;
            const next = this.nextSibling;
            parent.removeChild(this);
            for (const node of nodes) {
                const sibling = typeof node === "string" ? new WispText(node) : node;
                parent.insertBefore(sibling, next);
            }
        }

        remove() {
            if (this._parent) this._parent.removeChild(this);
        }

        insertAdjacentHTML(position, html) {
            const fragment = parseHtmlFragment(String(html));
            switch (String(position).toLowerCase()) {
                case "beforebegin":
                    if (!this._parent) return;
                    for (const child of fragment._children.slice()) {
                        this._parent.insertBefore(child, this);
                    }
                    break;
                case "afterbegin":
                    for (let index = fragment._children.length - 1; index >= 0; index--) {
                        this.insertBefore(fragment._children[index], this.firstChild);
                    }
                    break;
                case "beforeend":
                    for (const child of fragment._children.slice()) this.appendChild(child);
                    break;
                case "afterend":
                    if (!this._parent) return;
                    for (const child of fragment._children.slice()) {
                        this._parent.insertBefore(child, this.nextSibling);
                    }
                    break;
            }
        }

        insertAdjacentElement(position, element) {
            switch (String(position).toLowerCase()) {
                case "beforebegin":
                    if (this._parent) this._parent.insertBefore(element, this);
                    break;
                case "afterbegin":
                    this.insertBefore(element, this.firstChild);
                    break;
                case "beforeend":
                    this.appendChild(element);
                    break;
                case "afterend":
                    if (this._parent) this._parent.insertBefore(element, this.nextSibling);
                    break;
            }
            return element;
        }

        click() {
            this.dispatchEvent(new WispMouseEvent("click", { bubbles: true, cancelable: true }));
        }

        focus() {}
        blur() {}
        scrollIntoView() {}
        scrollTo() {}
        scrollBy() {}

        getBoundingClientRect() {
            return {
                x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0,
                width: 0, height: 0
            };
        }

        getClientRects() { return []; }
        get offsetWidth() { return 0; }
        get offsetHeight() { return 0; }
        get offsetTop() { return 0; }
        get offsetLeft() { return 0; }
        get clientWidth() { return 0; }
        get clientHeight() { return 0; }
        get scrollWidth() { return 0; }
        get scrollHeight() { return 0; }
        get scrollTop() { return 0; }
        set scrollTop(_value) {}
        get scrollLeft() { return 0; }
        set scrollLeft(_value) {}

        get sheet() { return null; }
        get content() { return null; }

        // Shadow DOM mínimo: los hijos del propio elemento hacen de sombra, de
        // modo que `connectedCallback()` puede pintar el componente sin que
        // Wisp necesite un árbol separado que no sabría serializar.
        attachShadow() {
            if (!this._shadowRoot) this._shadowRoot = this;
            return this._shadowRoot;
        }
        get shadowRoot() { return this._shadowRoot || null; }
        getRootNode() { return this; }

        getContext() { return null; }
        canPlayType() { return ""; }
        play() { return Promise.resolve(); }
        pause() {}
        load() {}

        getAttributeNS(_namespace, name) { return this.getAttribute(name); }
        setAttributeNS(_namespace, name, value) { this.setAttribute(name, value); }
        removeAttributeNS(_namespace, name) { this.removeAttribute(name); }
        hasAttributeNS(_namespace, name) { return this.hasAttribute(name); }
    }

    class WispTokenList {
        constructor(element, attribute) {
            this._element = element;
            this._attribute = attribute;
        }

        _values() {
            return (this._element.getAttribute(this._attribute) || "").split(/\s+/).filter(Boolean);
        }

        _write(values) {
            this._element.setAttribute(this._attribute, values.join(" "));
        }

        get length() { return this._values().length; }
        get value() { return this._element.getAttribute(this._attribute) || ""; }
        set value(text) { this._element.setAttribute(this._attribute, text); }
        item(index) { return this._values()[index] || null; }
        contains(name) { return this._values().includes(String(name)); }

        add(...names) {
            const values = this._values();
            for (const name of names) {
                const text = String(name);
                if (text && !values.includes(text)) values.push(text);
            }
            this._write(values);
        }

        remove(...names) {
            const remove = names.map(String);
            this._write(this._values().filter(name => !remove.includes(name)));
        }

        toggle(name, force) {
            const text = String(name);
            const present = this.contains(text);
            const next = force === undefined ? !present : !!force;
            if (next && !present) this.add(text);
            if (!next && present) this.remove(text);
            return next;
        }

        replace(oldName, newName) {
            const values = this._values();
            const index = values.indexOf(String(oldName));
            if (index < 0) return false;
            values[index] = String(newName);
            this._write(values);
            return true;
        }

        forEach(callback, thisArg) {
            this._values().forEach(callback, thisArg);
        }

        [Symbol.iterator]() {
            return this._values()[Symbol.iterator]();
        }

        toString() { return this.value; }
    }

    class WispDocument extends WispNode {
        constructor() {
            super(9);
            this._readyState = "loading";
            this._title = "";
            this.currentScript = null;
            this.visibilityState = "visible";
            this.hidden = false;
        }

        get nodeName() { return "#document"; }

        get doctype() { return null; }
        get readyState() { return this._readyState; }
        set readyState(value) { this._readyState = String(value); }
        get images() { return this.getElementsByTagName("img"); }
        get links() { return this.getElementsByTagName("a"); }

        get title() {
            const element = this.querySelector("title");
            return element ? element.textContent.trim() : this._title;
        }

        set title(value) {
            const text = value === null || value === undefined ? "" : String(value);
            this._title = text;
            let element = this.querySelector("title");
            if (!element) {
                const head = this.head;
                if (!head) return;
                element = this.createElement("title");
                head.appendChild(element);
            }
            element.textContent = text;
        }
        get documentElement() {
            for (const child of this._children) {
                if (child._type === 1 && child._tag === "html") return child;
            }
            return null;
        }
        get head() {
            const root = this.documentElement;
            if (!root) return null;
            for (const child of root._children) {
                if (child._type === 1 && child._tag === "head") return child;
            }
            return null;
        }
        get body() {
            const root = this.documentElement;
            if (!root) return null;
            for (const child of root._children) {
                if (child._type === 1 && child._tag === "body") return child;
            }
            return null;
        }
        get defaultView() { return globalThis; }
        get location() { return locationObject; }
        get URL() { return pageUrl.href || "about:blank"; }
        get documentURI() { return this.URL; }
        get characterSet() { return "UTF-8"; }
        get charset() { return "UTF-8"; }
        get contentType() { return "text/html"; }

        createElement(tag) {
            const element = new WispElement(tag);
            upgradeCustomElement(element);
            return element;
        }
        createElementNS(_namespace, tag) { return this.createElement(tag); }
        createTextNode(text) { return new WispText(text); }
        createComment(text) { return new WispComment(text); }
        createDocumentFragment() { return new WispDocumentFragment(); }
        createEvent() { return new WispEvent(""); }
        createRange() { return { selectNodeContents() {}, setStart() {}, setEnd() {}, toString() { return ""; } }; }

        getElementById(id) {
            const key = String(id);
            if (!key) return null;
            let found = null;
            walkElements(this, element => {
                if (!found && element.id === key) found = element;
            });
            return found;
        }

        querySelector(selector) { return queryAll(this, selector)[0] || null; }
        querySelectorAll(selector) { return queryAll(this, selector); }

        getElementsByTagName(tagName) {
            const tag = String(tagName).toLowerCase();
            const result = [];
            walkElements(this, element => {
                if (tag === "*" || element.localName === tag) result.push(element);
            });
            return result;
        }

        getElementsByClassName(className) {
            const classes = String(className).split(/\s+/).filter(Boolean);
            if (classes.length === 0) return [];
            const result = [];
            walkElements(this, element => {
                const own = element.className.split(/\s+/).filter(Boolean);
                if (classes.every(name => own.includes(name))) result.push(element);
            });
            return result;
        }

        getElementsByName(name) {
            const key = String(name);
            const result = [];
            walkElements(this, element => {
                if (element.getAttribute("name") === key) result.push(element);
            });
            return result;
        }

        elementFromPoint() { return null; }
        elementsFromPoint() { return []; }
        getSelection() {
            return { toString() { return ""; }, rangeCount: 0, removeAllRanges() {}, addRange() {} };
        }
        write() {}
        writeln() {}
        open() { return this; }
        close() {}
        execCommand() { return false; }
        queryCommandSupported() { return false; }
    }

    // Objeto global de la ventana. En Wisp el propio `globalThis` hace de
    // ventana, así que las propiedades leen de ahí; la clase existe para
    // describir la interfaz `Window` y para el objeto que despacha eventos.
    class WispWindow extends WispEventTarget {
        get window() { return globalThis; }
        get self() { return globalThis; }
        get document() { return documentObject; }
        get location() { return locationObject; }
        get innerWidth() { return viewport.width; }
        get innerHeight() { return viewport.height; }
        get closed() { return false; }
        get name() { return this._name || ""; }
        set name(value) { this._name = value === null || value === undefined ? "" : String(value); }
        get event() { return undefined; }
        alert() {}
        close() {}
        focus() {}
        blur() {}
        open() { return null; }
    }

    function parseAttributes(text, element) {
        const pattern = /([^\s"'>/=]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?/g;
        let match;
        while ((match = pattern.exec(text))) {
            const name = match[1].toLowerCase();
            const value = match[2] !== undefined ? match[2]
                : match[3] !== undefined ? match[3]
                : match[4] !== undefined ? match[4] : "";
            element.setAttribute(name, decodeEntities(value));
        }
    }

    function appendTextNode(parent, text) {
        if (text === "") return;
        const last = parent._children[parent._children.length - 1];
        if (last && last._type === 3) {
            last._text += text;
            return;
        }
        const node = new WispText(text);
        node._parent = parent;
        parent._children.push(node);
    }

    function parseHtmlFragment(html) {
        const root = new WispDocumentFragment();
        const stack = [root];
        let current = root;
        const pattern = /<!--([\s\S]*?)-->|<\/([a-zA-Z][\w:-]*)\s*>|<([a-zA-Z][\w:-]*)((?:\s+[^\s"'>/=]+(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^\s"'=<>`]+))?)*)\s*(\/?)>/g;
        let lastIndex = 0;
        let match;
        while ((match = pattern.exec(html))) {
            if (match.index > lastIndex) {
                appendTextNode(current, decodeEntities(html.slice(lastIndex, match.index)));
            }
            lastIndex = pattern.lastIndex;
            if (match[1] !== undefined) {
                const comment = new WispComment(match[1]);
                comment._parent = current;
                current._children.push(comment);
                continue;
            }
            if (match[2]) {
                const tag = match[2].toUpperCase();
                for (let index = stack.length - 1; index > 0; index--) {
                    if (stack[index]._tag.toUpperCase() === tag) {
                        stack.length = index;
                        current = stack[index - 1];
                        break;
                    }
                }
                continue;
            }
            const tag = match[3].toLowerCase();
            const element = new WispElement(tag);
            parseAttributes(match[4] || "", element);
            element._parent = current;
            current._children.push(element);
            if (!VOID_TAGS.has(tag) && match[5] !== "/") {
                stack.push(element);
                current = element;
            }
        }
        if (lastIndex < html.length) {
            appendTextNode(current, decodeEntities(html.slice(lastIndex)));
        }
        return root;
    }

    function serializeHtml(node) {
        if (node._type === 3) {
            if (node._parent && node._parent._type === 1 && RAW_TEXT_TAGS.has(node._parent._tag)) {
                return node._text;
            }
            return escapeText(node._text);
        }
        if (node._type === 8) return "<!--" + node._text + "-->";
        if (node._type === 11) {
            let html = "";
            for (const child of node._children) html += serializeHtml(child);
            return html;
        }
        let attributes = "";
        for (const [name, value] of node._attributes) {
            attributes += " " + name + '="' + escapeAttribute(value) + '"';
        }
        if (node._hasStyle && node._style.size > 0) {
            attributes += ' style="' + escapeAttribute(styleText(node)) + '"';
        }
        const tag = node._tag;
        if (VOID_TAGS.has(tag)) return "<" + tag + attributes + ">";
        let html = "";
        for (const child of node._children) html += serializeHtml(child);
        if (RAW_TEXT_TAGS.has(tag)) {
            return "<" + tag + attributes + ">" + html.replace(/<\/(script|style)/gi, "<\\/$1") + "</" + tag + ">";
        }
        return "<" + tag + attributes + ">" + html + "</" + tag + ">";
    }

    function base64Encode(text) {
        const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        const bytes = [];
        for (let index = 0; index < text.length; index++) {
            const code = text.charCodeAt(index);
            if (code > 255) throw new Error("btoa solo admite Latin-1");
            bytes.push(code);
        }
        let output = "";
        for (let index = 0; index < bytes.length; index += 3) {
            const a = bytes[index];
            const b = index + 1 < bytes.length ? bytes[index + 1] : 0;
            const c = index + 2 < bytes.length ? bytes[index + 2] : 0;
            output += alphabet[a >> 2];
            output += alphabet[((a & 3) << 4) | (b >> 4)];
            output += index + 1 < bytes.length ? alphabet[((b & 15) << 2) | (c >> 6)] : "=";
            output += index + 2 < bytes.length ? alphabet[c & 63] : "=";
        }
        return output;
    }

    function base64Decode(text) {
        const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        const clean = String(text).replace(/[^A-Za-z0-9+/=]/g, "");
        let output = "";
        for (let index = 0; index < clean.length; index += 4) {
            const a = alphabet.indexOf(clean[index]);
            const b = alphabet.indexOf(clean[index + 1]);
            const c = alphabet.indexOf(clean[index + 2]);
            const d = alphabet.indexOf(clean[index + 3]);
            output += String.fromCharCode((a << 2) | (b >> 4));
            if (c >= 0 && clean[index + 2] !== "=") output += String.fromCharCode(((b & 15) << 4) | (c >> 2));
            if (d >= 0 && clean[index + 3] !== "=") output += String.fromCharCode(((c & 3) << 6) | d);
        }
        return output;
    }

    const URL_PATTERN = /^(([^:/?#]+):)?(\/\/([^/?#]*))?([^?#]*)(\?([^#]*))?(#(.*))?$/;

    function urlParts(text) {
        const match = URL_PATTERN.exec(String(text));
        if (!match) return null;
        return {
            scheme: match[2] || "",
            authority: match[3] !== undefined ? (match[4] || "") : null,
            path: match[5] || "",
            query: match[7] !== undefined ? match[7] : null,
            fragment: match[9] !== undefined ? match[9] : null
        };
    }

    function normalizeUrlPath(path) {
        const leading = path.startsWith("/");
        const trailing = path.endsWith("/");
        const segments = [];
        for (const segment of path.split("/")) {
            if (segment === "" || segment === ".") continue;
            if (segment === "..") {
                segments.pop();
                continue;
            }
            segments.push(segment);
        }
        let result = (leading ? "/" : "") + segments.join("/");
        if (trailing && result !== "/") result += "/";
        if (result === "" && leading) result = "/";
        return result;
    }

    function parseAuthority(authority) {
        let login = "";
        let hostPort = String(authority);
        const at = hostPort.lastIndexOf("@");
        if (at >= 0) {
            login = hostPort.slice(0, at);
            hostPort = hostPort.slice(at + 1);
        }
        let username = "";
        let password = "";
        const colon = login.indexOf(":");
        if (colon >= 0) {
            username = login.slice(0, colon);
            password = login.slice(colon + 1);
        } else {
            username = login;
        }
        let hostname = hostPort;
        let port = "";
        if (hostPort.startsWith("[")) {
            const end = hostPort.indexOf("]");
            if (end >= 0) {
                hostname = hostPort.slice(0, end + 1);
                if (hostPort[end + 1] === ":") port = hostPort.slice(end + 2);
            }
        } else {
            const separator = hostPort.lastIndexOf(":");
            if (separator >= 0) {
                hostname = hostPort.slice(0, separator);
                port = hostPort.slice(separator + 1);
            }
        }
        return { username, password, hostname, port };
    }

    function resolveUrl(input, base) {
        const text = String(input);
        const parts = urlParts(text);
        if (!parts) throw new TypeError("URL no válida");
        if (parts.scheme) {
            return {
                scheme: parts.scheme.toLowerCase(),
                authority: parts.authority,
                path: normalizeUrlPath(parts.path),
                query: parts.query,
                fragment: parts.fragment
            };
        }
        if (base === undefined || base === null) throw new TypeError("URL no válida");
        const baseParts = urlParts(String(base));
        if (!baseParts || !baseParts.scheme) throw new TypeError("URL base no válida");
        const baseAuthority = baseParts.authority === null ? "" : "//" + baseParts.authority;
        let scheme = baseParts.scheme.toLowerCase();
        let authority = baseParts.authority;
        let path;
        let query = parts.query;
        let fragment = parts.fragment;
        if (text.startsWith("//")) {
            return {
                scheme,
                authority: parts.authority,
                path: normalizeUrlPath(parts.path),
                query,
                fragment
            };
        }
        if (parts.path.startsWith("/")) {
            path = normalizeUrlPath(parts.path);
        } else if (parts.path === "") {
            path = baseParts.path;
            if (query === null) query = baseParts.query;
        } else if (text.startsWith("?")) {
            path = baseParts.path;
        } else {
            const slash = baseParts.path.lastIndexOf("/");
            const directory = slash >= 0 ? baseParts.path.slice(0, slash + 1) : "/";
            path = normalizeUrlPath(directory + parts.path);
        }
        if (parts.path === "" && !text.startsWith("#")) {
            query = query === null ? baseParts.query : query;
        }
        return { scheme, authority, path, query, fragment };
    }

    class WispURLSearchParams {
        constructor(init) {
            this._entries = [];
            if (typeof init === "string") {
                for (const part of init.replace(/^\?/, "").split("&")) {
                    if (!part) continue;
                    const separator = part.indexOf("=");
                    const name = separator < 0 ? part : part.slice(0, separator);
                    const value = separator < 0 ? "" : part.slice(separator + 1);
                    this.append(decodeURIComponent(name.replace(/\+/g, " ")), decodeURIComponent(value.replace(/\+/g, " ")));
                }
            } else if (Array.isArray(init)) {
                for (const entry of init) {
                    if (entry && entry.length >= 2) this.append(entry[0], entry[1]);
                }
            } else if (init && typeof init === "object") {
                for (const key of Object.keys(init)) this.append(key, init[key]);
            }
        }

        get size() { return this._entries.length; }
        append(name, value) { this._entries.push([String(name), String(value)]); }
        delete(name) {
            const key = String(name);
            this._entries = this._entries.filter(entry => entry[0] !== key);
        }
        get(name) {
            const key = String(name);
            const entry = this._entries.find(item => item[0] === key);
            return entry ? entry[1] : null;
        }
        getAll(name) {
            const key = String(name);
            return this._entries.filter(item => item[0] === key).map(item => item[1]);
        }
        has(name) {
            const key = String(name);
            return this._entries.some(item => item[0] === key);
        }
        set(name, value) {
            const key = String(name);
            const index = this._entries.findIndex(item => item[0] === key);
            this._entries = this._entries.filter(item => item[0] !== key);
            if (index >= 0) this._entries.splice(index, 0, [key, String(value)]);
            else this._entries.push([key, String(value)]);
        }
        sort() {
            this._entries.sort((left, right) => left[0] < right[0] ? -1 : left[0] > right[0] ? 1 : 0);
        }
        forEach(callback, thisArg) {
            for (const entry of this._entries.slice()) callback.call(thisArg, entry[1], entry[0], this);
        }
        keys() { return this._entries.map(entry => entry[0])[Symbol.iterator](); }
        values() { return this._entries.map(entry => entry[1])[Symbol.iterator](); }
        entries() { return this._entries.map(entry => entry.slice())[Symbol.iterator](); }
        [Symbol.iterator]() { return this.entries(); }
        toString() {
            return this._entries
                .map(entry => encodeURIComponent(entry[0]) + "=" + encodeURIComponent(entry[1]))
                .join("&");
        }
    }

    class WispURL {
        constructor(input, base) {
            const parts = resolveUrl(input, base);
            if (!parts.scheme) throw new TypeError("URL no válida");
            const authority = parts.authority === null ? "" : parts.authority;
            const parsed = parseAuthority(authority);
            this.protocol = parts.scheme + ":";
            this.username = parsed.username;
            this.password = parsed.password;
            this.hostname = parsed.hostname;
            this.port = parsed.port;
            const defaultPort = (parts.scheme === "http" && this.port === "80")
                || (parts.scheme === "https" && this.port === "443");
            if (defaultPort) this.port = "";
            this.host = this.hostname + (this.port ? ":" + this.port : "");
            this.pathname = parts.path || (authority ? "/" : "");
            this.search = parts.query === null ? "" : "?" + parts.query;
            this.hash = parts.fragment === null ? "" : "#" + parts.fragment;
            this.origin = (parts.scheme === "http" || parts.scheme === "https") && this.hostname
                ? parts.scheme + "://" + this.host
                : "null";
            this._searchParams = null;
        }

        get href() {
            let text = this.protocol;
            if (this.hostname || this.protocol === "file:") text += "//";
            let login = "";
            if (this.username) login += this.username;
            if (this.password) login += ":" + this.password;
            if (login) text += login + "@";
            text += this.host + this.pathname + this.search + this.hash;
            return text;
        }

        set href(value) {
            const parts = resolveUrl(String(value), "about:blank");
            this.protocol = parts.scheme + ":";
            const parsed = parseAuthority(parts.authority === null ? "" : parts.authority);
            this.username = parsed.username;
            this.password = parsed.password;
            this.hostname = parsed.hostname;
            this.port = parsed.port;
            this.host = this.hostname + (this.port ? ":" + this.port : "");
            this.pathname = parts.path || "/";
            this.search = parts.query === null ? "" : "?" + parts.query;
            this.hash = parts.fragment === null ? "" : "#" + parts.fragment;
            this._searchParams = null;
        }

        get searchParams() {
            if (!this._searchParams) this._searchParams = new WispURLSearchParams(this.search);
            return this._searchParams;
        }

        toString() { return this.href; }
        toJSON() { return this.href; }
        static canParse(input, base) {
            try {
                new WispURL(input, base);
                return true;
            } catch (_) {
                return false;
            }
        }
    }

    class WispTextEncoder {
        get encoding() { return "utf-8"; }
        encode(text) {
            const input = String(text);
            const bytes = [];
            for (const character of input) {
                const code = character.codePointAt(0);
                if (code < 0x80) bytes.push(code);
                else if (code < 0x800) {
                    bytes.push(0xc0 | (code >> 6), 0x80 | (code & 0x3f));
                } else if (code < 0x10000) {
                    bytes.push(0xe0 | (code >> 12), 0x80 | ((code >> 6) & 0x3f), 0x80 | (code & 0x3f));
                } else {
                    bytes.push(
                        0xf0 | (code >> 18),
                        0x80 | ((code >> 12) & 0x3f),
                        0x80 | ((code >> 6) & 0x3f),
                        0x80 | (code & 0x3f)
                    );
                }
            }
            return new Uint8Array(bytes);
        }
    }

    class WispTextDecoder {
        constructor(label) {
            this.encoding = String(label || "utf-8").toLowerCase();
        }
        decode(buffer) {
            if (buffer === undefined || buffer === null) return "";
            const bytes = buffer instanceof Uint8Array ? buffer : new Uint8Array(buffer);
            let output = "";
            for (let index = 0; index < bytes.length;) {
                const first = bytes[index++];
                if (first < 0x80) {
                    output += String.fromCharCode(first);
                } else if (first < 0xe0) {
                    const second = bytes[index++] & 0x3f;
                    output += String.fromCharCode(((first & 0x1f) << 6) | second);
                } else if (first < 0xf0) {
                    const second = bytes[index++] & 0x3f;
                    const third = bytes[index++] & 0x3f;
                    output += String.fromCharCode(((first & 0x0f) << 12) | (second << 6) | third);
                } else {
                    const second = bytes[index++] & 0x3f;
                    const third = bytes[index++] & 0x3f;
                    const fourth = bytes[index++] & 0x3f;
                    output += String.fromCodePoint(
                        ((first & 0x07) << 18) | (second << 12) | (third << 6) | fourth
                    );
                }
            }
            return output;
        }
    }

    class WispStorage {
        constructor() { this._data = new Map(); }
        get length() { return this._data.size; }
        key(index) { const keys = Array.from(this._data.keys()); return keys[index] || null; }
        getItem(key) { const text = String(key); return this._data.has(text) ? this._data.get(text) : null; }
        setItem(key, value) { this._data.set(String(key), String(value)); }
        removeItem(key) { this._data.delete(String(key)); }
        clear() { this._data.clear(); }
    }

    function makeStorage() {
        const storage = new WispStorage();
        const methods = new Set(["length", "key", "getItem", "setItem", "removeItem", "clear"]);
        return new Proxy(storage, {
            get(target, property) {
                if (typeof property === "string" && methods.has(property)) {
                    const value = target[property];
                    return typeof value === "function" ? value.bind(target) : value;
                }
                return target.getItem(property);
            },
            set(target, property, value) {
                if (typeof property === "string" && methods.has(property)) return false;
                target.setItem(property, value);
                return true;
            },
            deleteProperty(target, property) {
                target.removeItem(property);
                return true;
            },
            has(target, property) {
                return target.getItem(property) !== null;
            },
            ownKeys(target) {
                return Array.from(target._data.keys());
            },
            getOwnPropertyDescriptor(target, property) {
                const value = target.getItem(property);
                if (value === null) return undefined;
                return { value, writable: true, enumerable: true, configurable: true };
            }
        });
    }

    const locationObject = {
        href: String(pageUrl.href || "about:blank"),
        origin: String(pageUrl.origin || "null"),
        protocol: String(pageUrl.protocol || "about:"),
        host: String(pageUrl.host || ""),
        hostname: String(pageUrl.hostname || ""),
        port: String(pageUrl.port || ""),
        pathname: String(pageUrl.pathname || ""),
        search: String(pageUrl.search || ""),
        hash: String(pageUrl.hash || ""),
        assign() {},
        replace() {},
        reload() {},
        toString() { return this.href; }
    };

    const historyObject = {
        length: 1,
        state: null,
        pushState(state) { this.state = state === undefined ? null : state; this.length++; },
        replaceState(state) { this.state = state === undefined ? null : state; },
        back() {},
        forward() {},
        go() {}
    };

    function performanceNow() {
        return Date.now() - performanceObject.timeOrigin;
    }

    const performanceObject = {
        timeOrigin: Date.now(),
        now: performanceNow
    };

    function scheduleTimer(callback, delay, argumentList, interval) {
        if (typeof callback !== "function") return 0;
        const id = ++timerSequence;
        const milliseconds = Math.max(0, Number(delay) || 0);
        timers.set(id, {
            id,
            callback,
            arguments: argumentList,
            due: performanceNow() + milliseconds,
            interval: interval ? Math.max(1, milliseconds) : 0,
            repeats: 0
        });
        return id;
    }

    function runTimers() {
        let callbacks = 0;
        while (callbacks < MAX_TIMER_CALLBACKS && timers.size > 0) {
            let next = null;
            for (const timer of timers.values()) {
                if (!next || timer.due < next.due || (timer.due === next.due && timer.id < next.id)) next = timer;
            }
            if (!next) break;
            timers.delete(next.id);
            callbacks++;
            try {
                next.callback.apply(globalThis, next.arguments);
            } catch (error) {
                reportError(error);
            }
            if (next.interval && next.repeats < MAX_INTERVAL_REPEATS) {
                next.repeats++;
                next.due = performanceNow() + next.interval;
                timers.set(next.id, next);
            }
        }
        let frames = 0;
        for (const [id, callback] of Array.from(animationFrames)) {
            if (frames >= MAX_ANIMATION_FRAMES) break;
            animationFrames.delete(id);
            frames++;
            try {
                callback(performanceNow());
            } catch (error) {
                reportError(error);
            }
        }
    }

    function finishDocument() {
        documentObject.readyState = "interactive";
        documentObject.dispatchEvent(new WispEvent("DOMContentLoaded", { bubbles: true }));
        runTimers();
        documentObject.readyState = "complete";
        windowTarget.dispatchEvent(new WispEvent("load", { bubbles: false }));
        runTimers();
    }

    function inspectTree(root) {
        const seen = new Set();
        const stack = [[root, 0]];
        let depth = 0;
        let cycle = false;
        while (stack.length > 0) {
            const [node, level] = stack.pop();
            if (seen.has(node)) {
                cycle = true;
                continue;
            }
            seen.add(node);
            if (level > depth) depth = level;
            for (const child of node._children) stack.push([child, level + 1]);
        }
        return { depth, cycle, count: seen.size };
    }

    function snapshot() {
        const root = documentObject.documentElement;
        if (!root) return { html: null, mutated: mutationCount > 0, oversized: false };
        const tree = inspectTree(root);
        if (tree.cycle || tree.depth > 256) {
            reportError(new Error(
                "El DOM generado por JavaScript es demasiado profundo o contiene un ciclo: "
                + tree.depth + " niveles, " + tree.count + " nodos."
            ));
            return { html: null, mutated: mutationCount > 0, oversized: false };
        }
        let html;
        try {
            html = serializeHtml(root);
        } catch (error) {
            reportError(error);
            return { html: null, mutated: mutationCount > 0, oversized: false };
        }
        if (html.length > MAX_SNAPSHOT_BYTES) {
            runtimeWarnings.push("El DOM generado por JavaScript supera el límite de tamaño de Wisp.");
            return { html: null, mutated: mutationCount > 0, oversized: true };
        }
        return { html, mutated: mutationCount > 0, oversized: false };
    }

    function takeWarnings() {
        return runtimeWarnings.splice(0, runtimeWarnings.length);
    }

    windowTarget = new WispWindow();
    documentObject = new WispDocument();

    // Build the initial tree without marking every node as a mutation.
    function buildInitial(data) {
        if (data.text !== null && data.text !== undefined) {
            return new WispText(data.text);
        }
        const element = new WispElement(data.tag);
        const attributes = data.attributes || {};
        for (const name of Object.keys(attributes)) {
            const key = String(name).toLowerCase();
            const value = String(attributes[name]);
            if (key === "style") {
                element._style = parseStyle(value);
                element._hasStyle = true;
            } else {
                element._attributes.set(key, value);
            }
        }
        for (const child of data.children || []) {
            const node = buildInitial(child);
            node._parent = element;
            element._children.push(node);
        }
        return element;
    }

    const initialRoot = buildInitial(__wispDocument);
    initialRoot._parent = documentObject;
    documentObject._children.push(initialRoot);

    const titleElement = documentObject.querySelector("title");
    documentObject._title = titleElement ? titleElement.textContent.trim() : "";
    documentObject.readyState = "interactive";

    globalThis.document = documentObject;
    globalThis.Node = WispNode;
    globalThis.Element = WispElement;
    globalThis.HTMLElement = WispElement;
    globalThis.Document = WispDocument;
    globalThis.HTMLDocument = WispDocument;
    globalThis.Window = WispWindow;
    globalThis.Text = WispText;
    globalThis.Comment = WispComment;
    globalThis.DocumentFragment = WispDocumentFragment;
    globalThis.Event = WispEvent;
    globalThis.CustomEvent = WispCustomEvent;
    globalThis.MouseEvent = WispMouseEvent;
    globalThis.KeyboardEvent = WispKeyboardEvent;
    globalThis.EventTarget = WispEventTarget;
    globalThis.location = locationObject;
    globalThis.history = historyObject;
    globalThis.performance = performanceObject;
    globalThis.localStorage = makeStorage();
    globalThis.sessionStorage = makeStorage();
    globalThis.innerWidth = viewport.width;
    globalThis.innerHeight = viewport.height;
    globalThis.outerWidth = viewport.width;
    globalThis.outerHeight = viewport.height;
    globalThis.devicePixelRatio = 1;
    globalThis.scrollX = 0;
    globalThis.scrollY = 0;
    globalThis.pageXOffset = 0;
    globalThis.pageYOffset = 0;
    globalThis.screen = {
        width: viewport.width,
        height: viewport.height,
        availWidth: viewport.width,
        availHeight: viewport.height,
        colorDepth: 24,
        pixelDepth: 24
    };
    globalThis.navigator = Object.freeze({
        userAgent: "Mozilla/5.0 (compatible; Wisp/0.1)",
        appName: "Netscape",
        appVersion: "5.0 (Wisp/0.1)",
        platform: "Win32",
        language: "es-ES",
        languages: Object.freeze(["es-ES", "es", "en"]),
        onLine: true,
        cookieEnabled: false,
        doNotTrack: null,
        hardwareConcurrency: 1,
        maxTouchPoints: 0
    });
    globalThis.console = {
        log() {}, info() {}, debug() {}, warn() {}, error() {}, trace() {},
        group() {}, groupEnd() {}, table() {}, dir() {}, assert() {},
        time() {}, timeEnd() {}, count() {}, countReset() {}, clear() {}
    };
    globalThis.setTimeout = function (callback, delay, ...args) {
        return scheduleTimer(callback, delay, args, false);
    };
    globalThis.setInterval = function (callback, delay, ...args) {
        return scheduleTimer(callback, delay, args, true);
    };
    globalThis.clearTimeout = function (id) { timers.delete(Number(id)); };
    globalThis.clearInterval = globalThis.clearTimeout;
    globalThis.setImmediate = function (callback, ...args) {
        return scheduleTimer(callback, 0, args, false);
    };
    globalThis.clearImmediate = globalThis.clearTimeout;
    globalThis.queueMicrotask = function (callback) {
        Promise.resolve().then(callback).catch(reportError);
    };
    globalThis.requestAnimationFrame = function (callback) {
        if (typeof callback !== "function") return 0;
        const id = ++timerSequence;
        animationFrames.set(id, callback);
        return id;
    };
    globalThis.cancelAnimationFrame = function (id) { animationFrames.delete(Number(id)); };
    globalThis.requestIdleCallback = function (callback) {
        return scheduleTimer(() => callback({
            didTimeout: false,
            timeRemaining() { return 0; }
        }), 1, [], false);
    };
    globalThis.cancelIdleCallback = globalThis.clearTimeout;
    globalThis.alert = function () {};
    globalThis.confirm = function () { return false; };
    globalThis.prompt = function () { return null; };
    globalThis.scrollTo = function () {};
    globalThis.scrollBy = function () {};
    globalThis.scroll = function () {};
    globalThis.open = function () { return null; };
    globalThis.close = function () {};
    globalThis.focus = function () {};
    globalThis.blur = function () {};
    globalThis.getSelection = function () { return documentObject.getSelection(); };
    globalThis.btoa = base64Encode;
    globalThis.atob = base64Decode;
    globalThis.URL = WispURL;
    globalThis.URLSearchParams = WispURLSearchParams;
    globalThis.TextEncoder = WispTextEncoder;
    globalThis.TextDecoder = WispTextDecoder;
    globalThis.structuredClone = function (value) {
        if (value === undefined) return undefined;
        return JSON.parse(JSON.stringify(value));
    };
    globalThis.addEventListener = windowTarget.addEventListener.bind(windowTarget);
    globalThis.removeEventListener = windowTarget.removeEventListener.bind(windowTarget);
    globalThis.dispatchEvent = windowTarget.dispatchEvent.bind(windowTarget);
    globalThis.window = globalThis;
    globalThis.self = globalThis;
    globalThis.top = globalThis;
    globalThis.parent = globalThis;
    globalThis.frames = globalThis;
    globalThis.getComputedStyle = function (element) {
        if (!element || !element._style) {
            return { getPropertyValue() { return ""; }, display: "", visibility: "" };
        }
        const fallback = {
            display: BLOCK_TAGS.has(element._tag) ? "block" : "inline",
            visibility: "visible",
            opacity: "1"
        };
        const style = element.style;
        const proxy = new Proxy({}, {
            get(_target, property) {
                if (typeof property !== "string") return undefined;
                const key = cssName(property);
                const value = style.getPropertyValue(key);
                return value || fallback[key] || "";
            }
        });
        return proxy;
    };
    globalThis.matchMedia = function (query) {
        const text = String(query);
        let matched = true;
        let evaluated = false;
        const width = viewport.width;
        const height = viewport.height;
        for (const condition of text.split(/\s+and\s+/i)) {
            const minWidth = condition.match(/min-width\s*:\s*(\d+(?:\.\d+)?)px/i);
            const maxWidth = condition.match(/max-width\s*:\s*(\d+(?:\.\d+)?)px/i);
            const minHeight = condition.match(/min-height\s*:\s*(\d+(?:\.\d+)?)px/i);
            const maxHeight = condition.match(/max-height\s*:\s*(\d+(?:\.\d+)?)px/i);
            if (minWidth) {
                evaluated = true;
                matched = matched && width >= Number(minWidth[1]);
            } else if (maxWidth) {
                evaluated = true;
                matched = matched && width <= Number(maxWidth[1]);
            } else if (minHeight) {
                evaluated = true;
                matched = matched && height >= Number(minHeight[1]);
            } else if (maxHeight) {
                evaluated = true;
                matched = matched && height <= Number(maxHeight[1]);
            } else if (/orientation\s*:\s*portrait/i.test(condition)) {
                evaluated = true;
                matched = matched && height >= width;
            } else if (/orientation\s*:\s*landscape/i.test(condition)) {
                evaluated = true;
                matched = matched && width > height;
            } else if (/prefers-color-scheme\s*:\s*dark/i.test(condition)) {
                evaluated = true;
                matched = false;
            } else if (/prefers-color-scheme\s*:\s*light/i.test(condition)) {
                evaluated = true;
            } else if (/prefers-reduced-motion\s*:\s*no-preference/i.test(condition)) {
                evaluated = true;
            } else if (/prefers-reduced-motion\s*:\s*reduce/i.test(condition)) {
                evaluated = true;
                matched = false;
            }
        }
        if (!evaluated) matched = false;
        return {
            matches: matched,
            media: text,
            onchange: null,
            addListener() {},
            removeListener() {},
            addEventListener() {},
            removeEventListener() {},
            dispatchEvent() { return false; }
        };
    };
    globalThis.Image = function Image() {
        return documentObject.createElement("img");
    };
    globalThis.Audio = function Audio() {
        return documentObject.createElement("audio");
    };
    globalThis.MutationObserver = class MutationObserver {
        constructor(callback) { this._callback = callback; }
        observe() {}
        disconnect() {}
        takeRecords() { return []; }
    };
    globalThis.ResizeObserver = class ResizeObserver {
        constructor(callback) { this._callback = callback; }
        observe() {}
        unobserve() {}
        disconnect() {}
    };
    globalThis.IntersectionObserver = class IntersectionObserver {
        constructor(callback) {
            this._callback = callback;
            this._observed = [];
        }
        observe(target) {
            this._observed.push(target);
            scheduleTimer(() => {
                if (!this._observed.includes(target)) return;
                try {
                    this._callback([{
                        target,
                        isIntersecting: true,
                        intersectionRatio: 1,
                        boundingClientRect: target.getBoundingClientRect(),
                        intersectionRect: target.getBoundingClientRect(),
                        rootBounds: null,
                        time: performanceNow()
                    }], this);
                } catch (error) {
                    reportError(error);
                }
            }, 0, [], false);
        }
        unobserve(target) {
            this._observed = this._observed.filter(item => item !== target);
        }
        disconnect() { this._observed = []; }
        takeRecords() { return []; }
    };
    globalThis.customElements = {
        define(name, constructor) {
            const tag = String(name).toLowerCase();
            if (!/^[a-z][a-z0-9._]*-[a-z0-9._-]*$/.test(tag)) {
                throw new TypeError(
                    "customElements.define: el nombre debe llevar guion y minúsculas"
                );
            }
            if (typeof constructor !== "function") {
                throw new TypeError("customElements.define: se esperaba una clase");
            }
            if (customDefinitions.has(tag)) {
                throw new Error("customElements.define: <" + tag + "> ya está definido");
            }
            const observed = Array.isArray(constructor.observedAttributes)
                ? constructor.observedAttributes.map(attribute => String(attribute).toLowerCase())
                : [];
            const definition = { constructor, observed };
            customDefinitions.set(tag, definition);
            // Mejora los elementos que ya estaban en el documento (HTML
            // analizado antes de que corriera el script que los define).
            const pending = [];
            walkElements(documentObject, element => {
                if (!element._definition && element._tag === tag) pending.push(element);
            });
            for (const element of pending) upgradeCustomElement(element);
            const waiters = customDefinitionWaiters.get(tag);
            if (waiters) {
                customDefinitionWaiters.delete(tag);
                for (const resolve of waiters) resolve(constructor);
            }
        },
        get(name) {
            const definition = customDefinitions.get(String(name).toLowerCase());
            return definition ? definition.constructor : undefined;
        },
        whenDefined(name) {
            const tag = String(name).toLowerCase();
            if (!/^[a-z][a-z0-9._]*-[a-z0-9._-]*$/.test(tag)) {
                return Promise.reject(
                    new TypeError("customElements.whenDefined: nombre no válido")
                );
            }
            const definition = customDefinitions.get(tag);
            if (definition) return Promise.resolve(definition.constructor);
            return new Promise(resolve => {
                const waiters = customDefinitionWaiters.get(tag) || [];
                waiters.push(resolve);
                customDefinitionWaiters.set(tag, waiters);
            });
        },
        upgrade(root) {
            const start = root || documentObject;
            if (start._type === 1) upgradeCustomElement(start);
            walkElements(start, element => upgradeCustomElement(element));
        }
    };
    globalThis.CSS = {
        supports() { return false; },
        escape(text) { return String(text).replace(/[^a-zA-Z0-9_-]/g, character => "\\" + character); }
    };
    globalThis.crypto = {
        getRandomValues(array) {
            for (let index = 0; index < array.length; index++) {
                array[index] = Math.floor(Math.random() * 256);
            }
            return array;
        },
        randomUUID() {
            const hex = "0123456789abcdef";
            let value = "";
            for (let index = 0; index < 36; index++) {
                if (index === 8 || index === 13 || index === 18 || index === 23) value += "-";
                else if (index === 14) value += "4";
                else value += hex[Math.floor(Math.random() * 16)];
            }
            return value;
        }
    };

    Object.defineProperty(globalThis, "__wispFinish", {
        value: finishDocument,
        writable: false,
        enumerable: false,
        configurable: false
    });
    Object.defineProperty(globalThis, "__wispSnapshot", {
        value: snapshot,
        writable: false,
        enumerable: false,
        configurable: false
    });
    // ------------------------------------------------------- fetch / XHR
    // La red la hace Rust (`__wispFetchSync`): Boa no tiene E/S. La respuesta
    // se convierte en un objeto tipo Response con `text()`/`json()` que
    // devuelven promesas, así el código de las webs funciona sin cambios.
    function wispResponseFromJson(raw) {
        const data = JSON.parse(raw);
        const headers = new Map();
        if (data.type) headers.set("content-type", String(data.type).toLowerCase());
        return {
            ok: Boolean(data.ok),
            status: Number(data.status) || 0,
            statusText: "",
            url: String(data.url || locationObject.href),
            redirected: false,
            type: "basic",
            headers: {
                get(name) {
                    const key = String(name).toLowerCase();
                    return headers.has(key) ? headers.get(key) : null;
                },
                has(name) { return headers.has(String(name).toLowerCase()); },
                forEach(callback) { headers.forEach((value, key) => callback(value, key)); }
            },
            text() { return Promise.resolve(String(data.body)); },
            json() {
                return new Promise((resolve, reject) => {
                    try { resolve(JSON.parse(String(data.body))); }
                    catch (error) { reject(error); }
                });
            },
            clone() { return wispResponseFromJson(raw); }
        };
    }

    function wispFetch(input, init) {
        return new Promise((resolve, reject) => {
            try {
                const url = typeof input === "string" ? input : (input && input.url) || String(input);
                const method = (init && init.method) || "GET";
                const raw = typeof __wispFetchSync === "function"
                    ? __wispFetchSync(url, String(method))
                    : null;
                if (raw === null || raw === undefined) {
                    reject(new TypeError("Failed to fetch"));
                    return;
                }
                // Wisp devuelve `{ error }` cuando CORS, un límite o la red
                // bloquean la petición: se propaga el motivo, no un fallo mudo.
                const failure = JSON.parse(raw);
                if (failure && failure.error) {
                    const error = new TypeError(String(failure.error));
                    reportError(error);
                    reject(error);
                    return;
                }
                resolve(wispResponseFromJson(raw));
            } catch (error) {
                reject(error);
            }
        });
    }
    globalThis.fetch = wispFetch;

    class WispXMLHttpRequest {
        constructor() {
            this.readyState = 0;
            this.status = 0;
            this.statusText = "";
            this.responseText = "";
            this.response = "";
            this.responseType = "";
            this.onload = null;
            this.onerror = null;
            this.onreadystatechange = null;
            this._listeners = new Map();
            this._method = "GET";
            this._url = "";
            this._headers = new Map();
        }
        open(method, url) {
            this._method = String(method || "GET");
            this._url = String(url);
            this.readyState = 1;
            this._emit("readystatechange");
        }
        setRequestHeader() {}
        getResponseHeader(name) {
            const key = String(name).toLowerCase();
            return this._headers.has(key) ? this._headers.get(key) : null;
        }
        getAllResponseHeaders() {
            return Array.from(this._headers.entries())
                .map(([key, value]) => `${key}: ${value}`)
                .join("\r\n");
        }
        addEventListener(type, listener) {
            if (typeof listener !== "function") return;
            const key = String(type);
            const list = this._listeners.get(key) || [];
            list.push(listener);
            this._listeners.set(key, list);
        }
        removeEventListener(type, listener) {
            const list = this._listeners.get(String(type));
            if (!list) return;
            const index = list.indexOf(listener);
            if (index >= 0) list.splice(index, 1);
        }
        _emit(type) {
            const event = { type, target: this, currentTarget: this };
            const handler = this["on" + type];
            if (typeof handler === "function") handler.call(this, event);
            for (const listener of (this._listeners.get(type) || []).slice()) {
                listener.call(this, event);
            }
        }
        abort() {}
        send() {
            const raw = typeof __wispFetchSync === "function"
                ? __wispFetchSync(this._url, this._method)
                : null;
            if (raw === null || raw === undefined) {
                this.readyState = 4;
                this.status = 0;
                this._emit("readystatechange");
                this._emit("error");
                this._emit("loadend");
                return;
            }
            const data = JSON.parse(raw);
            if (data && data.error) {
                const error = new TypeError(String(data.error));
                reportError(error);
                this.readyState = 4;
                this.status = 0;
                this.statusText = String(data.error);
                this._emit("readystatechange");
                this._emit("error");
                this._emit("loadend");
                return;
            }
            this.status = Number(data.status) || 0;
            this.responseText = String(data.body);
            this.response = this.responseType === "json"
                ? JSON.parse(this.responseText)
                : this.responseText;
            if (data.type) this._headers.set("content-type", String(data.type).toLowerCase());
            this.readyState = 4;
            this._emit("readystatechange");
            this._emit("load");
            this._emit("loadend");
        }
    }
    globalThis.XMLHttpRequest = WispXMLHttpRequest;

    Object.defineProperty(globalThis, "__wispTakeWarnings", {
        value: takeWarnings,
        writable: false,
        enumerable: false,
        configurable: false
    });
    Object.defineProperty(globalThis, "__wispDocument", {
        value: undefined,
        writable: false,
        enumerable: false,
        configurable: false
    });
    Object.defineProperty(globalThis, "__wispUrl", {
        value: undefined,
        writable: false,
        enumerable: false,
        configurable: false
    });
    Object.defineProperty(globalThis, "__wispViewport", {
        value: undefined,
        writable: false,
        enumerable: false,
        configurable: false
    });
})();
