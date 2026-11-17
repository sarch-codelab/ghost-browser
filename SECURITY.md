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
- **Wisp**: el JavaScript de las páginas se ejecuta con Boa bajo límites
  estrictos (presupuesto de tiempo, iteraciones, recursión, pila) y sin
  acceso a red desde el script. El filtrado de dominios es una capa de
  privacidad básica, no una garantía.
- **Sesión**: los datos de perfil y sesión se guardan en el directorio de
  datos de la aplicación del usuario.
