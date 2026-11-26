//! Descriptores de interfaz WebIDL para el DOM de Wisp.
//!
//! Este crate no implementa el DOM: solo describe las interfaces que Wisp
//! expone a JavaScript, a partir de los fragmentos `.webidl` de `webidl/`.
//! `build.rs` los parsea y genera las tablas de [`INTERFACES`]; aquí viven los
//! tipos de esos descriptores y unas cuantas consultas de conveniencia
//! (`interface`, `ancestors`, `inherits`) pensadas para que las etapas
//! posteriores (bindings y puentes nativos) trabajen contra una única fuente
//! de verdad en vez de listas escritas a mano.

#![forbid(unsafe_code)]

/// Una interfaz del DOM, con sus miembros y su herencia.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interface {
    /// Nombre de la interfaz (p. ej. `Element`).
    pub name: &'static str,
    /// Interfaz de la que hereda, si la hay.
    pub parent: Option<&'static str>,
    /// Contextos donde se expone (`[Exposed=...]`); `["*"]` significa todos.
    pub exposed: &'static [&'static str],
    /// Nombre del objeto global si la interfaz lo define (`[Global=...]`).
    pub global: Option<&'static str>,
    /// Si la interfaz declara uno o más `constructor(...)`.
    pub constructible: bool,
    /// Constantes declaradas con `const`.
    pub constants: &'static [Constant],
    /// Atributos (propiedades) de la interfaz.
    pub attributes: &'static [Attribute],
    /// Operaciones (métodos) de la interfaz.
    pub operations: &'static [Operation],
}

/// Una constante declarada en una interfaz (`const unsigned short ELEMENT_NODE = 1;`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Constant {
    pub name: &'static str,
    /// Tipo declarado, tal cual aparece en el IDL (p. ej. `unsigned short`).
    pub type_: &'static str,
    /// Valor literal, como texto (p. ej. `1`).
    pub value: &'static str,
}

/// Un atributo (propiedad) de una interfaz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attribute {
    pub name: &'static str,
    /// Tipo declarado, sin el sufijo `?` (que se refleja en [`Attribute::nullable`]).
    pub type_: &'static str,
    pub nullable: bool,
    pub readonly: bool,
    /// `[SameObject]`: el getter devuelve siempre el mismo objeto.
    pub same_object: bool,
    /// `[Replaceable]`: la propiedad puede reemplazarse con una asignación.
    pub replaceable: bool,
    /// `[LegacyUnforgeable]`: la propiedad no se puede eliminar ni sobrescribir.
    pub legacy_unforgeable: bool,
    /// `[CEReactions]`: los cambios disparan reacciones de elementos personalizados.
    pub cereactions: bool,
}

/// Una operación (método) de una interfaz.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operation {
    pub name: &'static str,
    /// Tipo de retorno, sin el sufijo `?` (que se refleja en [`Operation::return_nullable`]).
    pub return_type: &'static str,
    pub return_nullable: bool,
    pub cereactions: bool,
    pub arguments: &'static [Argument],
}

/// Un argumento de una operación.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Argument {
    pub name: &'static str,
    /// Tipo declarado, sin `?` ni `...`.
    pub type_: &'static str,
    /// Declarado como `optional`.
    pub optional: bool,
    /// Declarado como variádico (`DOMString... text`).
    pub variadic: bool,
    pub nullable: bool,
    /// Valor por defecto, como texto (p. ej. `{}` o `""`).
    pub default: Option<&'static str>,
}

include!(concat!(env!("OUT_DIR"), "/descriptors.rs"));

impl Interface {
    /// Busca un atributo propio (sin contar los heredados).
    pub fn attribute(&self, name: &str) -> Option<&Attribute> {
        self.attributes.iter().find(|a| a.name == name)
    }

    /// Busca una operación propia (sin contar las heredadas).
    pub fn operation(&self, name: &str) -> Option<&Operation> {
        self.operations.iter().find(|o| o.name == name)
    }

    /// Busca una constante propia.
    pub fn constant(&self, name: &str) -> Option<&Constant> {
        self.constants.iter().find(|c| c.name == name)
    }

    /// Interfaz padre, resuelta contra [`INTERFACES`].
    pub fn parent_interface(&self) -> Option<&'static Interface> {
        self.parent.and_then(interface)
    }
}

/// Devuelve la interfaz con ese nombre, si existe.
pub fn interface(name: &str) -> Option<&'static Interface> {
    INTERFACES.iter().find(|i| i.name == name)
}

/// Nombres de todas las interfaces conocidas, en el orden declarado.
pub fn interface_names() -> Vec<&'static str> {
    INTERFACES.iter().map(|i| i.name).collect()
}

/// Cadena de ancestros de una interfaz, de padre inmediato hacia arriba.
pub fn ancestors(name: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut current = interface(name).and_then(|i| i.parent);
    while let Some(parent_name) = current {
        out.push(parent_name);
        current = interface(parent_name).and_then(|i| i.parent);
    }
    out
}

/// Indica si `child` hereda (directa o indirectamente) de `ancestor`.
pub fn inherits(child: &str, ancestor: &str) -> bool {
    ancestors(child).contains(&ancestor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str) -> &'static Interface {
        interface(name).unwrap_or_else(|| panic!("falta la interfaz `{name}`"))
    }

    #[test]
    fn genera_las_seis_interfaces() {
        assert_eq!(INTERFACES.len(), 6);
        let mut names = interface_names();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "Document",
                "Element",
                "EventTarget",
                "HTMLElement",
                "Node",
                "Window",
            ]
        );
        assert!(interface("Inexistente").is_none());
    }

    #[test]
    fn modela_la_herencia() {
        assert_eq!(iface("EventTarget").parent, None);
        assert_eq!(iface("Node").parent, Some("EventTarget"));
        assert_eq!(iface("Element").parent, Some("Node"));
        assert_eq!(iface("HTMLElement").parent, Some("Element"));
        assert_eq!(iface("Document").parent, Some("Node"));
        assert_eq!(iface("Window").parent, Some("EventTarget"));

        assert_eq!(
            ancestors("HTMLElement"),
            vec!["Element", "Node", "EventTarget"]
        );
        assert!(inherits("HTMLElement", "EventTarget"));
        assert!(inherits("Document", "Node"));
        assert!(!inherits("Node", "Element"));
        assert!(!inherits("EventTarget", "EventTarget"));
    }

    #[test]
    fn event_target_es_construible_y_registra_listeners() {
        let target = iface("EventTarget");
        assert!(target.constructible);
        assert_eq!(target.exposed, &["*"]);

        let add = target.operation("addEventListener").unwrap();
        assert_eq!(add.return_type, "undefined");
        assert_eq!(add.arguments.len(), 3);

        let callback = &add.arguments[1];
        assert_eq!(callback.name, "callback");
        assert_eq!(callback.type_, "EventListener");
        assert!(callback.nullable);

        let options = &add.arguments[2];
        assert!(options.optional);
        assert_eq!(options.type_, "(AddEventListenerOptions or boolean)");
        assert_eq!(options.default, Some("{}"));

        let dispatch = target.operation("dispatchEvent").unwrap();
        assert_eq!(dispatch.return_type, "boolean");
        assert_eq!(dispatch.arguments[0].name, "event");
    }

    #[test]
    fn node_expone_constantes_y_atributos_readonly() {
        let node = iface("Node");
        assert_eq!(node.constants.len(), 3);
        let element_node = node.constant("ELEMENT_NODE").unwrap();
        assert_eq!(element_node.type_, "unsigned short");
        assert_eq!(element_node.value, "1");
        assert_eq!(node.constant("TEXT_NODE").unwrap().value, "3");

        let node_type = node.attribute("nodeType").unwrap();
        assert!(node_type.readonly);
        assert_eq!(node_type.type_, "unsigned short");
        assert!(!node_type.nullable);

        let node_value = node.attribute("nodeValue").unwrap();
        assert!(node_value.nullable);
        assert_eq!(node_value.type_, "DOMString");

        let append = node.operation("appendChild").unwrap();
        assert_eq!(append.return_type, "Node");
        assert!(!append.return_nullable);

        let clone = node.operation("cloneNode").unwrap();
        assert!(clone.return_nullable);
        assert!(clone.arguments[0].optional);
        assert_eq!(clone.arguments[0].default, Some("false"));
    }

    #[test]
    fn element_marca_los_objetos_compartidos() {
        let element = iface("Element");
        let class_list = element.attribute("classList").unwrap();
        assert!(class_list.same_object);
        assert!(class_list.readonly);

        let attributes = element.attribute("attributes").unwrap();
        assert!(attributes.same_object);

        let id = element.attribute("id").unwrap();
        assert!(!id.readonly);
        assert!(!id.same_object);

        let query = element.operation("querySelector").unwrap();
        assert_eq!(query.return_type, "Element");
        assert!(query.return_nullable);

        let insert = element.operation("insertAdjacentHTML").unwrap();
        assert_eq!(insert.arguments.len(), 2);

        let toggle = element.operation("toggleAttribute").unwrap();
        assert!(toggle.arguments[1].optional);
        assert_eq!(toggle.arguments[1].default, None);
    }

    #[test]
    fn htmlelement_declara_cereactions() {
        let html = iface("HTMLElement");
        for name in ["title", "lang", "hidden", "dir"] {
            let attr = html.attribute(name).unwrap();
            assert!(attr.cereactions, "`{name}` debería tener [CEReactions]");
        }
        assert!(!html.attribute("innerText").unwrap().cereactions);
        assert_eq!(html.operation("click").unwrap().arguments.len(), 0);
    }

    #[test]
    fn document_admite_write_variadico() {
        let document = iface("Document");
        let write = document.operation("write").unwrap();
        assert_eq!(write.arguments.len(), 1);
        assert!(write.arguments[0].variadic);
        assert_eq!(write.arguments[0].type_, "DOMString");

        assert_eq!(
            document.operation("createTextNode").unwrap().return_type,
            "Text"
        );
        assert_eq!(
            document.operation("createElement").unwrap().return_type,
            "Element"
        );
        assert!(document.attribute("location").unwrap().legacy_unforgeable);
        assert!(document.attribute("doctype").unwrap().nullable);
    }

    #[test]
    fn window_es_el_objeto_global() {
        let window = iface("Window");
        assert_eq!(window.global, Some("Window"));
        assert_eq!(window.exposed, &["Window"]);
        assert!(!window.constructible);

        assert!(window.attribute("document").unwrap().same_object);
        assert!(window.attribute("event").unwrap().replaceable);

        let open = window.operation("open").unwrap();
        assert_eq!(open.arguments.len(), 3);
        assert!(open.arguments.iter().all(|a| a.optional));
        assert_eq!(open.arguments[0].default, Some("\"\""));
        assert!(open.return_nullable);
    }
}
