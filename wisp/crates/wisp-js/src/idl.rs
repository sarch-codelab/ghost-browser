//! Conformidad entre los descriptores WebIDL (`wisp-idl`) y el runtime DOM nativo.

use anyhow::Result;

/// Presencia de una interfaz descrita en el IDL dentro del runtime de Wisp.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InterfaceStatus {
    pub name: &'static str,
    pub present: bool,
}

/// Informe de conformidad entre `wisp-idl` y el runtime DOM.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct IdlConformance {
    /// Estado de cada interfaz descrita, en el orden del IDL.
    pub interfaces: Vec<InterfaceStatus>,
    /// Miembros ausentes, con la forma `Interfaz.miembro` o `Interfaz.metodo()`.
    pub missing_members: Vec<String>,
    /// Total de atributos y operaciones comprobados.
    pub total_members: usize,
    /// Miembros encontrados en el runtime.
    pub found_members: usize,
}

impl IdlConformance {
    /// Nombres de las interfaces presentes en el runtime.
    pub fn present_interfaces(&self) -> Vec<&'static str> {
        self.interfaces
            .iter()
            .filter(|status| status.present)
            .map(|status| status.name)
            .collect()
    }

    /// Nombres de las interfaces descritas que el runtime todavía no instala.
    pub fn missing_interfaces(&self) -> Vec<&'static str> {
        self.interfaces
            .iter()
            .filter(|status| !status.present)
            .map(|status| status.name)
            .collect()
    }
}

/// Compara los descriptores del IDL con el runtime DOM nativo de `wisp-js`.
pub fn idl_conformance() -> Result<IdlConformance> {
    let mut interfaces = Vec::with_capacity(wisp_idl::INTERFACES.len());
    let mut missing_members = Vec::new();
    let mut total_members = 0usize;
    let mut found_members = 0usize;

    for iface in wisp_idl::INTERFACES {
        let present = crate::native_dom::native_idl_supports_interface(iface.name);
        interfaces.push(InterfaceStatus {
            name: iface.name,
            present,
        });
        if !present {
            continue;
        }
        for attr in iface.attributes {
            total_members += 1;
            if crate::native_dom::native_idl_supports_member(iface.name, attr.name) {
                found_members += 1;
            } else {
                missing_members.push(format!("{}.{}", iface.name, attr.name));
            }
        }
        for op in iface.operations {
            total_members += 1;
            if crate::native_dom::native_idl_supports_member(iface.name, op.name) {
                found_members += 1;
            } else {
                missing_members.push(format!("{}.{}()", iface.name, op.name));
            }
        }
    }

    Ok(IdlConformance {
        interfaces,
        missing_members,
        total_members,
        found_members,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubre_todas_las_interfaces_descritas() {
        let report = idl_conformance().expect("el chequeo debe ejecutarse");
        assert_eq!(report.interfaces.len(), wisp_idl::INTERFACES.len());
    }

    #[test]
    fn detecta_las_interfaces_que_dom_js_ya_instala() {
        let report = idl_conformance().expect("el chequeo debe ejecutarse");
        let present = report.present_interfaces();
        for name in [
            "EventTarget",
            "Node",
            "Element",
            "HTMLElement",
            "Document",
            "Window",
        ] {
            assert!(
                present.contains(&name),
                "`{name}` debería estar presente en el DOM de Wisp"
            );
        }
    }

    #[test]
    fn dom_js_satisface_todos_los_miembros_descritos() {
        let report = idl_conformance().expect("el chequeo debe ejecutarse");
        assert_eq!(
            report.missing_members,
            Vec::<String>::new(),
            "el DOM de Wisp debe implementar cada atributo y operación descritos"
        );
    }

    #[test]
    fn el_informe_es_coherente() {
        let report = idl_conformance().expect("el chequeo debe ejecutarse");
        assert_eq!(
            report.missing_members.len(),
            report.total_members - report.found_members,
            "cada miembro debe contar como encontrado o ausente"
        );
        assert!(report.found_members > 0, "el DOM sí implementa miembros");
    }
}
