//! Despachador CDP headless y log de eventos en streaming para `wisp-agent`
//! (ítems 19 y 20 del backlog).
//!
//! Cubre los dominios pedidos por `puppeteer-core`, `playwright-core` y agentes:
//! `Target`, `Page`, `Runtime`, `DOM`, `Network`, `Fetch`, `IO`, `Storage`,
//! `Input` y `LP.getMarkdown`, emitiendo eventos (`Runtime.consoleAPICalled`,
//! `Network.requestWillBeSent`, `Page.frameNavigated`, `Page.screencastFrame`).

use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
}

impl CdpEvent {
    pub fn to_json(&self) -> Value {
        json!({
            "method": self.method,
            "params": self.params,
        })
    }
}

pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
        out.push(TABLE[b0 >> 2] as char);
        out.push(TABLE[((b0 & 0x03) << 4) | (b1 >> 4)] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((b1 & 0x0f) << 2) | (b2 >> 6)] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[b2 & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}
