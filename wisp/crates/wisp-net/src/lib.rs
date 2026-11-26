//! Capa de red y presupuestos duros de Wisp (ítems 23 y 24 del backlog).
//!
//! - **SSRF (ítem 23)**: bloquea IPs privadas/loopback/link-local/metadata por
//!   defecto, comprobando tanto IPs literales como las IPs resultantes de la
//!   resolución DNS (defensa frente a *DNS rebinding*). `--allow-private-network`
//!   o `WISP_ALLOW_PRIVATE_NETWORK=1` habilita redes locales cuando hace falta.
//! - **Presupuestos duros (ítem 24)**: `WISP_SCRIPT_DEADLINE_MS`,
//!   `WISP_MODULE_BUDGET_MS`, `WISP_FETCH_DEADLINE_MS` y watchdog determinista.

#![forbid(unsafe_code)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

/// Política de red y presupuestos de ejecución.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkPolicy {
    pub allow_private_network: bool,
    pub fetch_deadline_ms: u64,
    pub script_deadline_ms: u64,
    pub module_budget_ms: u64,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self::strict()
    }
}

impl NetworkPolicy {
    /// Política segura por defecto: bloquea IPs privadas y fija presupuestos duros.
    pub fn strict() -> Self {
        Self {
            allow_private_network: false,
            fetch_deadline_ms: 8_000,
            script_deadline_ms: 120,
            module_budget_ms: 500,
        }
    }

    /// Política permisiva (solo para tests locales explícitos o flag `--allow-private-network`).
    pub fn allow_private() -> Self {
        Self {
            allow_private_network: true,
            ..Self::strict()
        }
    }

    /// Lee `WISP_ALLOW_PRIVATE_NETWORK`, `WISP_FETCH_DEADLINE_MS`,
    /// `WISP_SCRIPT_DEADLINE_MS` y `WISP_MODULE_BUDGET_MS` del entorno.
    pub fn from_env() -> Self {
        let mut policy = Self::strict();
        if let Ok(val) = std::env::var("WISP_ALLOW_PRIVATE_NETWORK") {
            let v = val.trim().to_ascii_lowercase();
            policy.allow_private_network = matches!(v.as_str(), "1" | "true" | "yes");
        }
        if let Some(ms) = env_u64("WISP_FETCH_DEADLINE_MS") {
            policy.fetch_deadline_ms = ms;
        }
        if let Some(ms) = env_u64("WISP_SCRIPT_DEADLINE_MS") {
            policy.script_deadline_ms = ms;
        }
        if let Some(ms) = env_u64("WISP_MODULE_BUDGET_MS") {
            policy.module_budget_ms = ms;
        }
        policy
    }
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse::<u64>().ok()
}

/// Deadline determinista para presupuestos de script, módulos o fetch.
#[derive(Debug, Clone, Copy)]
pub struct BudgetDeadline {
    expires_at: Instant,
}

impl BudgetDeadline {
    pub fn from_millis(ms: u64) -> Self {
        Self {
            expires_at: Instant::now() + Duration::from_millis(ms),
        }
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    pub fn remaining(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }
}

/// Indica si `ip` pertenece a un rango privado, loopback, link-local,
/// CGNAT, sin especificar o reservado para metadatos cloud (`169.254.169.254`).
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_ipv4(v4),
        IpAddr::V6(v6) => is_private_ipv6(v6),
    }
}

fn is_private_ipv4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_unspecified()
        || v4.is_multicast()
        // 100.64.0.0/10 (CGNAT RFC 6598)
        || (o[0] == 100 && (o[1] & 0b1100_0000) == 64)
        // 192.0.0.0/24, 198.18.0.0/15 (benchmarking), 240.0.0.0/4 (reserved)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        || o[0] >= 240
}

fn is_private_ipv6(v6: Ipv6Addr) -> bool {
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return is_private_ipv4(mapped);
    }
    let segs = v6.segments();
    v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        // fc00::/7 (Unique Local Address)
        || (segs[0] & 0xfe00) == 0xfc00
        // fe80::/10 (Link-local)
        || (segs[0] & 0xffc0) == 0xfe80
}

/// Valida `host` y resuelve DNS cuando no es una IP literal, rechazando
/// cualquier dirección privada si `!policy.allow_private_network`.
pub fn validate_host_with_resolver<F>(
    host: &str,
    port: u16,
    policy: &NetworkPolicy,
    resolve: F,
) -> Result<Vec<SocketAddr>, String>
where
    F: FnOnce(&str, u16) -> Result<Vec<SocketAddr>, String>,
{
    let host_clean = host.trim().trim_start_matches('[').trim_end_matches(']');
    if host_clean.is_empty() {
        return Err("host vacío".into());
    }

    if let Ok(ip) = host_clean.parse::<IpAddr>() {
        if !policy.allow_private_network && is_private_ip(ip) {
            return Err(format!(
                "SSRF bloqueado: IP privada o local `{ip}` (usa --allow-private-network para permitirla)"
            ));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    if !policy.allow_private_network && host_clean.eq_ignore_ascii_case("localhost") {
        return Err("SSRF bloqueado: `localhost` requiere --allow-private-network".into());
    }

    let addrs = resolve(host_clean, port)?;
    if addrs.is_empty() {
        return Err(format!("DNS no devolvió direcciones para `{host_clean}`"));
    }
    if !policy.allow_private_network {
        for addr in &addrs {
            if is_private_ip(addr.ip()) {
                return Err(format!(
                    "SSRF bloqueado (DNS rebinding): `{host_clean}` resolvió a IP privada {}",
                    addr.ip()
                ));
            }
        }
    }
    Ok(addrs)
}

/// Valida una URL http(s) usando el resolver DNS del sistema operativo.
pub fn validate_url(url: &str, policy: &NetworkPolicy) -> Result<Vec<SocketAddr>, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| "URL sin esquema".to_string())?;
    let scheme_lower = scheme.to_ascii_lowercase();
    if scheme_lower != "http" && scheme_lower != "https" {
        return Err(format!("esquema no permitido `{scheme_lower}`"));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err("URL con credenciales embebidas no permitida".into());
    }
    let default_port = if scheme_lower == "https" { 443 } else { 80 };
    let (host, port) = split_host_port(authority, default_port)?;
    validate_host_with_resolver(host, port, policy, |h, p| {
        (h, p)
            .to_socket_addrs()
            .map(|iter| iter.collect())
            .map_err(|e| format!("error resolviendo DNS de `{h}`: {e}"))
    })
}

fn split_host_port(authority: &str, default_port: u16) -> Result<(&str, u16), String> {
    if authority.starts_with('[') {
        let end = authority
            .find(']')
            .ok_or_else(|| "dirección IPv6 mal formada".to_string())?;
        let host = &authority[1..end];
        let tail = &authority[end + 1..];
        let port = if let Some(p) = tail.strip_prefix(':') {
            p.parse::<u16>()
                .map_err(|_| "puerto inválido en URL".to_string())?
        } else {
            default_port
        };
        return Ok((host, port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => {
            let port = p
                .parse::<u16>()
                .map_err(|_| "puerto inválido en URL".to_string())?;
            Ok((h, port))
        }
        None => Ok((authority, default_port)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_private_and_metadata_ips_by_default() {
        let strict = NetworkPolicy::strict();
        for url in [
            "http://127.0.0.1:8080/",
            "http://localhost/admin",
            "http://10.0.0.1/",
            "http://172.16.5.4/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]:3000/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(
                validate_url(url, &strict).is_err(),
                "`{url}` debe quedar bloqueada sin --allow-private-network"
            );
        }
    }

    #[test]
    fn allows_private_ips_with_flag() {
        let allowed = NetworkPolicy::allow_private();
        let addrs = validate_url("http://127.0.0.1:47820/health", &allowed).unwrap();
        assert_eq!(addrs.len(), 1);
    }

    #[test]
    fn detects_dns_rebinding_to_private_ip() {
        let strict = NetworkPolicy::strict();
        let rebind = validate_host_with_resolver("evil.example", 80, &strict, |_, port| {
            Ok(vec![
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), port),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port),
            ])
        });
        assert!(rebind.is_err(), "debe rechazar rebinding a 127.0.0.1");

        let public = validate_host_with_resolver("example.com", 443, &strict, |_, port| {
            Ok(vec![SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
                port,
            )])
        });
        assert!(public.is_ok());
    }

    #[test]
    fn deadline_expires_deterministically() {
        let expired = BudgetDeadline::from_millis(0);
        assert!(expired.expired());
    }
}
