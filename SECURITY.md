# Política de seguridad

## Versiones soportadas

| Versión | Estado            |
| ------- | ----------------- |
| 0.1.x   | Soporte actual    |
| < 0.1   | Sin soporte       |

## Reportar una vulnerabilidad

**No abras un issue público** para reportar vulnerabilidades de seguridad.

En su lugar:

1. Envía un correo al mantenedor describiendo la vulnerabilidad, los pasos
   para reproducirla y el impacto potencial.
2. Espera confirmación antes de divulgarla públicamente.

## Superficie de seguridad actual

- **API remota**: escucha solo en `127.0.0.1:47820` (o `GHOST_REMOTE_PORT`).
  No acepta conexiones externas. No hay autenticación porque el socket es
  loopback; si ejecutas Ghost en un equipo compartido, ten en cuenta que
  cualquier proceso local puede controlar el navegador.
- **WebView2**: las páginas corren en el sandbox de Chromium con las
  protecciones estándar del runtime.
- **Wisp**: el JavaScript de las páginas se ejecuta bajo límites estrictos
  (presupuesto de tiempo, iteraciones, recursión, pila) con `fetch()`/`XHR`
  acotados por CORS y presupuesto de red. El crate `wisp-net` bloquea por
  defecto peticiones SSRF hacia IPs privadas, loopback (`127.0.0.1`, `::1`),
  link-local/metadata (`169.254.169.254`) y *DNS rebinding* tanto en la
  descarga principal como en redirecciones HTTP, subrecursos (CSS, `@import`,
  scripts, módulos, imágenes) y `fetch()` de JS (desactivable solo con
  `--allow-private-network` o `WISP_ALLOW_PRIVATE_NETWORK=1`). El filtrado de
  dominios en Ghost es una capa de privacidad básica, no una garantía.
- **Sesión**: los datos de perfil y sesión se guardan en el directorio de
  datos de la aplicación del usuario.
