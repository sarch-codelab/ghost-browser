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
    pub stealth: bool,
    pub block_trackers: bool,
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
            stealth: false,
            block_trackers: false,
        }
    }

    /// Política permisiva (solo para tests locales explícitos o flag `--allow-private-network`).
    pub fn allow_private() -> Self {
        Self {
            allow_private_network: true,
            ..Self::strict()
        }
    }

    /// Lee `WISP_ALLOW_PRIVATE_NETWORK`, `WISP_STEALTH`, `WISP_BLOCK_TRACKERS`,
    /// `WISP_FETCH_DEADLINE_MS`, `WISP_SCRIPT_DEADLINE_MS` y `WISP_MODULE_BUDGET_MS`.
    pub fn from_env() -> Self {
        let mut policy = Self::strict();
        if let Some(b) = env_bool("WISP_ALLOW_PRIVATE_NETWORK") {
            policy.allow_private_network = b;
        }
        if let Some(b) = env_bool("WISP_STEALTH") {
            policy.stealth = b;
        }
        if let Some(b) = env_bool("WISP_BLOCK_TRACKERS") {
            policy.block_trackers = b;
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

fn env_bool(key: &str) -> Option<bool> {
    let val = std::env::var(key).ok()?;
    let v = val.trim().to_ascii_lowercase();
    Some(matches!(v.as_str(), "1" | "true" | "yes"))
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

/// Perfil de cabeceras HTTP para el modo `--stealth` o el perfil estándar de Wisp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StealthProfile {
    pub user_agent: &'static str,
    pub accept: &'static str,
    pub accept_language: &'static str,
    pub sec_ch_ua: Option<&'static str>,
    pub sec_ch_ua_mobile: Option<&'static str>,
    pub sec_ch_ua_platform: Option<&'static str>,
    pub sec_fetch_dest: Option<&'static str>,
    pub sec_fetch_mode: Option<&'static str>,
    pub sec_fetch_site: Option<&'static str>,
    pub upgrade_insecure_requests: bool,
}

impl StealthProfile {
    pub const fn wisp_default() -> Self {
        Self {
            user_agent: "GhostWisp/0.1",
            accept: "text/html,application/xhtml+xml",
            accept_language: "en-US,en;q=0.9",
            sec_ch_ua: None,
            sec_ch_ua_mobile: None,
            sec_ch_ua_platform: None,
            sec_fetch_dest: None,
            sec_fetch_mode: None,
            sec_fetch_site: None,
            upgrade_insecure_requests: false,
        }
    }

    pub const fn chrome_desktop() -> Self {
        Self {
            user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
            accept: "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
            accept_language: "en-US,en;q=0.9,es;q=0.8",
            sec_ch_ua: Some(
                "\"Google Chrome\";v=\"131\", \"Chromium\";v=\"131\", \"Not_A Brand\";v=\"24\"",
            ),
            sec_ch_ua_mobile: Some("?0"),
            sec_ch_ua_platform: Some("\"Windows\""),
            sec_fetch_dest: Some("document"),
            sec_fetch_mode: Some("navigate"),
            sec_fetch_site: Some("none"),
            upgrade_insecure_requests: true,
        }
    }

    pub const fn for_policy(policy: &NetworkPolicy) -> Self {
        if policy.stealth {
            Self::chrome_desktop()
        } else {
            Self::wisp_default()
        }
    }
}

const MULTI_LABEL_PUBLIC_SUFFIXES: &[&str] = &[
    "co.uk",
    "org.uk",
    "gov.uk",
    "ac.uk",
    "net.uk",
    "com.au",
    "net.au",
    "org.au",
    "edu.au",
    "co.jp",
    "ne.jp",
    "or.jp",
    "ac.jp",
    "com.br",
    "org.br",
    "net.br",
    "com.mx",
    "org.mx",
    "com.ar",
    "com.hn",
    "gob.hn",
    "co.in",
    "net.in",
    "org.in",
    "com.cn",
    "net.cn",
    "org.cn",
    "com.tw",
    "co.nz",
    "co.za",
    "com.sg",
    "com.tr",
    "co.kr",
    "com.es",
    "com.co",
    "com.pe",
];

/// Devuelve el dominio registrable (eTLD+1) para un host normalizado.
/// Para IPs o hosts de una sola etiqueta devuelve el host en minúsculas.
pub fn registrable_domain(host: &str) -> String {
    let clean = host
        .trim()
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if clean.is_empty() || clean.parse::<IpAddr>().is_ok() {
        return clean;
    }
    let labels: Vec<&str> = clean.split('.').filter(|s| !s.is_empty()).collect();
    if labels.len() <= 2 {
        return labels.join(".");
    }
    let last_two = format!("{}.{}", labels[labels.len() - 2], labels[labels.len() - 1]);
    if MULTI_LABEL_PUBLIC_SUFFIXES.contains(&last_two.as_str()) {
        if labels.len() >= 3 {
            return format!("{}.{}", labels[labels.len() - 3], last_two);
        }
        return last_two;
    }
    last_two
}

fn is_public_suffix(domain: &str) -> bool {
    let clean = domain
        .trim()
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if clean.is_empty() || clean.parse::<IpAddr>().is_ok() {
        return false;
    }
    !clean.contains('.') || MULTI_LABEL_PUBLIC_SUFFIXES.contains(&clean.as_str())
}

const TRACKER_HOSTS: &[&str] = &[
    "google-analytics.com",
    "googletagmanager.com",
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "connect.facebook.net",
    "analytics.tiktok.com",
    "hotjar.com",
    "mixpanel.com",
    "segment.io",
    "segment.com",
    "amplitude.com",
    "sentry.io",
    "scorecardresearch.com",
    "quantserve.com",
    "taboola.com",
    "outbrain.com",
    "criteo.com",
    "ads-twitter.com",
    "adservice.google.com",
];

pub fn is_tracker_host(host: &str) -> bool {
    let clean = host.trim().trim_end_matches('.').to_ascii_lowercase();
    TRACKER_HOSTS.iter().any(|tracker| {
        let suffix = format!(".{tracker}");
        clean == *tracker || clean.ends_with(&suffix)
    })
}

pub fn is_tracker_url(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = split_host_port(authority, 80)
        .map(|(h, _)| h)
        .unwrap_or(authority);
    is_tracker_host(host)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieEntry {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub host_only: bool,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SameSite,
}

/// Almacén de cookies con validación eTLD+1 (rechaza cookies sobre sufijos
/// públicos como `.co.uk` o `.com` y valida coincidencia de dominio y ruta).
#[derive(Debug, Default, Clone)]
pub struct CookieJar {
    entries: Vec<CookieEntry>,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_from_header(
        &mut self,
        request_host: &str,
        request_path: &str,
        is_https: bool,
        set_cookie_header: &str,
    ) -> bool {
        let mut parts = set_cookie_header.split(';');
        let Some(name_value) = parts.next() else {
            return false;
        };
        let Some((raw_name, raw_val)) = name_value.split_once('=') else {
            return false;
        };
        let name = raw_name.trim();
        if name.is_empty() {
            return false;
        }
        let value = raw_val.trim();
        let req_host_lower = request_host.trim().to_ascii_lowercase();
        let mut domain = req_host_lower.clone();
        let mut host_only = true;
        let mut path = default_cookie_path(request_path);
        let mut secure = false;
        let mut http_only = false;
        let mut same_site = SameSite::Lax;
        let mut max_age_zero = false;

        for attr in parts {
            let trimmed = attr.trim();
            if trimmed.eq_ignore_ascii_case("secure") {
                secure = true;
            } else if trimmed.eq_ignore_ascii_case("httponly") {
                http_only = true;
            } else if let Some((k, v)) = trimmed.split_once('=') {
                let k_low = k.trim().to_ascii_lowercase();
                let v_trim = v.trim();
                match k_low.as_str() {
                    "domain" => {
                        let d = v_trim.trim_start_matches('.').to_ascii_lowercase();
                        if d.is_empty() || is_public_suffix(&d) {
                            return false;
                        }
                        if !domain_matches(&req_host_lower, &d)
                            || registrable_domain(&req_host_lower) != registrable_domain(&d)
                        {
                            return false;
                        }
                        domain = d;
                        host_only = false;
                    }
                    "path" => {
                        if v_trim.starts_with('/') {
                            path = v_trim.to_owned();
                        }
                    }
                    "samesite" => match v_trim.to_ascii_lowercase().as_str() {
                        "strict" => same_site = SameSite::Strict,
                        "none" => same_site = SameSite::None,
                        _ => same_site = SameSite::Lax,
                    },
                    "max-age" => {
                        if v_trim.parse::<i64>().is_ok_and(|s| s <= 0) {
                            max_age_zero = true;
                        }
                    }
                    _ => {}
                }
            }
        }

        if same_site == SameSite::None && !secure {
            return false;
        }
        if secure && !is_https {
            // Cookies Secure solo aplican a HTTPS (almacenarlas solo si origen seguro).
            return false;
        }

        let same_key = |c: &CookieEntry| c.name == name && c.domain == domain && c.path == path;
        self.entries.retain(|c| !same_key(c));
        if max_age_zero {
            return true;
        }
        self.entries.push(CookieEntry {
            name: name.to_owned(),
            value: value.to_owned(),
            domain,
            path,
            host_only,
            secure,
            http_only,
            same_site,
        });
        true
    }

    pub fn cookie_header_for(
        &self,
        host: &str,
        path: &str,
        is_https: bool,
        is_same_site: bool,
    ) -> Option<String> {
        let host_lower = host.trim().to_ascii_lowercase();
        let req_path = if path.is_empty() { "/" } else { path };
        let mut pairs = Vec::new();
        for c in &self.entries {
            if c.secure && !is_https {
                continue;
            }
            if !is_same_site && c.same_site != SameSite::None {
                continue;
            }
            let domain_ok = if c.host_only {
                host_lower == c.domain
            } else {
                domain_matches(&host_lower, &c.domain)
            };
            if !domain_ok || !path_matches(req_path, &c.path) {
                continue;
            }
            pairs.push(format!("{}={}", c.name, c.value));
        }
        if pairs.is_empty() {
            None
        } else {
            Some(pairs.join("; "))
        }
    }

    pub fn entries_for_host(&self, host: &str) -> Vec<&CookieEntry> {
        let host_lower = host.trim().to_ascii_lowercase();
        self.entries
            .iter()
            .filter(|c| {
                if c.host_only {
                    host_lower == c.domain
                } else {
                    domain_matches(&host_lower, &c.domain)
                }
            })
            .collect()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

fn default_cookie_path(request_path: &str) -> String {
    if !request_path.starts_with('/') || request_path == "/" {
        return "/".to_owned();
    }
    match request_path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(idx) => request_path[..idx].to_owned(),
    }
}

fn domain_matches(host: &str, domain: &str) -> bool {
    if host == domain {
        return true;
    }
    host.ends_with(&format!(".{domain}"))
}

fn path_matches(req_path: &str, cookie_path: &str) -> bool {
    if req_path == cookie_path {
        return true;
    }
    if req_path.starts_with(cookie_path) {
        return cookie_path.ends_with('/') || req_path[cookie_path.len()..].starts_with('/');
    }
    false
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

    #[test]
    fn registrable_domain_handles_public_suffixes() {
        assert_eq!(registrable_domain("sub.example.com"), "example.com");
        assert_eq!(registrable_domain("app.service.co.uk"), "service.co.uk");
        assert_eq!(registrable_domain("127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn cookie_jar_enforces_etld_plus_one_and_samesite() {
        let mut jar = CookieJar::new();
        assert!(!jar.set_from_header("a.co.uk", "/", true, "sid=bad; Domain=co.uk"));
        assert!(!jar.set_from_header("a.example.com", "/", true, "sid=bad; Domain=com"));
        let other = jar.set_from_header("a.example.com", "/", true, "sid=bad; Domain=other.com");
        assert!(!other);
        let ok = jar.set_from_header(
            "app.example.com",
            "/v1/home",
            true,
            "sid=ok123; Domain=example.com; Path=/; Secure; HttpOnly; SameSite=Lax",
        );
        assert!(ok);
        let hdr = jar.cookie_header_for("sub.example.com", "/v1/page", true, true);
        assert_eq!(hdr.as_deref(), Some("sid=ok123"));
        let missing_http = jar.cookie_header_for("sub.example.com", "/v1/page", false, true);
        assert_eq!(missing_http, None);
        let missing_script = jar.cookie_header_for("sub.example.com", "/v1/page", true, false);
        assert_eq!(missing_script, None);
    }

    #[test]
    fn tracker_blocker_matches_exact_and_subdomains() {
        assert!(is_tracker_url("https://www.google-analytics.com/analytics.js"));
        assert!(is_tracker_url("https://doubleclick.net/pixel"));
        assert!(!is_tracker_url("https://example.com/?ref=doubleclick.net"));
    }
}
