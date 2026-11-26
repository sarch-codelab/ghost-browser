//! Conformidad entre los descriptores WebIDL (`wisp-idl`) y el runtime DOM.
//!
//! Con la feature por defecto (motor propio `wisp_js::vm` + `native_dom`),
//! contrasta los descriptores generados desde `wisp-idl` con las capacidades
//! nativas de `NativeDomHost`. Con `legacy-boa`, conserva además la comprobación
//! histórica de `dom.js` en Boa.

#[cfg(feature = "legacy-boa")]
use crate::DOM_RUNTIME;
#[cfg(not(feature = "legacy-boa"))]
use anyhow::Result;
#[cfg(feature = "legacy-boa")]
use anyhow::{Result, anyhow};
#[cfg(feature = "legacy-boa")]
use boa_engine::property::Attribute;
#[cfg(feature = "legacy-boa")]
use boa_engine::{Context, JsValue, Source, js_string};
#[cfg(feature = "legacy-boa")]
use serde::Deserialize;

#[cfg(feature = "legacy-boa")]
const IDL_CHECK: &str = include_str!("idl_check.js");

/// Presencia de una interfaz descrita en el IDL dentro del runtime de Wisp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceStatus {
    pub name: &'static str,
    pub present: bool,
}

/// Informe de conformidad entre `wisp-idl` y el runtime DOM.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[cfg(not(feature = "legacy-boa"))]
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

#[cfg(feature = "legacy-boa")]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReport {
    missing_interfaces: Vec<String>,
    missing_members: Vec<String>,
    total_members: usize,
    found_members: usize,
}

#[cfg(feature = "legacy-boa")]
/// Compara los descriptores del IDL con el DOM que instala `dom.js` (Boa).
pub fn idl_conformance() -> Result<IdlConformance> {
    let mut context = Context::builder()
        .build()
        .map_err(|error| anyhow!("no se pudo crear el contexto Boa: {error}"))?;

    register_json(
        &mut context,
        "__wispDocument",
        serde_json::json!({ "tag": "html", "children": [] }),
    )?;
    register_json(&mut context, "__wispUrl", serde_json::json!({}))?;
    register_json(
        &mut context,
        "__wispViewport",
        serde_json::json!({ "width": 1024 }),
    )?;

    context
        .eval(Source::from_bytes(DOM_RUNTIME))
        .map_err(|error| anyhow!("no se pudo iniciar el DOM de Wisp: {error}"))?;

    register_json(&mut context, "__wispIdl", descriptor_json())?;

    let value = context
        .eval(Source::from_bytes(IDL_CHECK))
        .map_err(|error| anyhow!("falló el chequeo de conformidad: {error}"))?;
    let json = value
        .to_json(&mut context)
        .map_err(|error| anyhow!("no se pudo leer el informe de conformidad: {error}"))?
        .ok_or_else(|| anyhow!("el chequeo de conformidad no devolvió datos"))?;
    let raw: RawReport = serde_json::from_value(json)
        .map_err(|error| anyhow!("informe de conformidad ilegible: {error}"))?;

    let interfaces = wisp_idl::INTERFACES
        .iter()
        .map(|iface| InterfaceStatus {
            name: iface.name,
            present: !raw.missing_interfaces.iter().any(|name| name == iface.name),
        })
        .collect();

    Ok(IdlConformance {
        interfaces,
        missing_members: raw.missing_members,
        total_members: raw.total_members,
        found_members: raw.found_members,
    })
}

#[cfg(feature = "legacy-boa")]
fn register_json(context: &mut Context, name: &str, value: serde_json::Value) -> Result<()> {
    let value = JsValue::from_json(&value, context)
        .map_err(|error| anyhow!("no se pudo preparar `{name}`: {error}"))?;
    context
        .register_global_property(js_string!(name), value, Attribute::all())
        .map_err(|error| anyhow!("no se pudo registrar `{name}`: {error}"))
}

#[cfg(feature = "legacy-boa")]
/// Vista mínima de los descriptores que consume el chequeo en JavaScript.
fn descriptor_json() -> serde_json::Value {
    serde_json::Value::Array(
        wisp_idl::INTERFACES
            .iter()
            .map(|iface| {
                serde_json::json!({
                    "name": iface.name,
                    "attributes": iface.attributes.iter().map(|attr| attr.name).collect::<Vec<_>>(),
                    "operations": iface.operations.iter().map(|op| op.name).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
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
