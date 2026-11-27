// Smoke test real de conectividad CDP con Playwright.
//
//   node cdp-smoke.mjs
//
// Arranca `wisp-agent serve`, abre una página con la API HTTP, conecta con
// `chromium.connectOverCDP` y comprueba qué partes del protocolo usa ya
// Playwright de verdad.
//
// Las comprobaciones "required" son las que definen si la conexión funciona.
// Las "diag" documentan el estado del ciclo de vida de página (goto, título,
// locators): hoy Playwright no puede navegar porque necesita un modelo de
// sesiones por frame que `wisp-agent` todavía no expone. Se informan, no
// tumban el test.
//
// Código de salida: 0 si todo lo required pasa; 1 si algo required falla.

import { spawn } from 'node:child_process';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright-core';

const here = path.dirname(fileURLToPath(import.meta.url));
const binary = process.env.WISP_AGENT_BIN
  ?? path.resolve(here, '../../../..', 'target/debug',
    process.platform === 'win32' ? 'wisp-agent.exe' : 'wisp-agent');
const port = Number(process.env.WISP_CDP_PORT ?? 47899);
const endpoint = `ws://127.0.0.1:${port}/devtools/browser/wisp`;

const FIXTURE = [
  '<!doctype html><html><head><title>Wisp CDP</title></head><body>',
  '<h1 id="h">Hola</h1>',
  '<input id="q" name="q" value="">',
  '<a id="link" href="/next">next</a>',
  '</body></html>',
].join('');

const results = [];
function record(name, ok, detail = '', required = true) {
  results.push({ name, ok, required });
  const tag = ok ? 'PASS' : required ? 'FAIL' : 'DIAG';
  console.log(`${tag} ${name}${detail ? ` — ${detail}` : ''}`);
}

function startFixtureServer() {
  return new Promise((resolve) => {
    const server = http.createServer((request, response) => {
      response.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
      response.end(FIXTURE);
    });
    server.listen(0, '127.0.0.1', () => {
      resolve({ server, url: `http://127.0.0.1:${server.address().port}/` });
    });
  });
}

async function waitForHealth(deadlineMs = 20_000) {
  const started = Date.now();
  while (Date.now() - started < deadlineMs) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/health`);
      if (response.ok) return true;
    } catch {
      // el servidor aún no escucha
    }
    await new Promise((done) => setTimeout(done, 150));
  }
  return false;
}

const fixture = await startFixtureServer();
// El fixture vive en 127.0.0.1: hay que habilitar la red privada, igual que
// haría un navegador de escritorio al abrir localhost.
const agent = spawn(
  binary,
  ['serve', '--port', String(port), '--workers', '1', '--allow-private-network'],
  {
    stdio: ['ignore', 'pipe', 'pipe'],
  },
);
let agentLog = '';
agent.stdout.on('data', (chunk) => { agentLog += chunk; });
agent.stderr.on('data', (chunk) => { agentLog += chunk; });

// Watchdog: si un paso del protocolo se cuelga, el test falla en vez de
// quedarse esperando (Playwright espera indefinidamente a algunos eventos).
const watchdog = setTimeout(() => {
  console.error('FAIL watchdog — la conexión CDP dejó de responder');
  if (process.env.WISP_CDP_TRACE) console.error(agentLog);
  agent.kill();
  process.exit(2);
}, Number(process.env.WISP_CDP_TIMEOUT ?? 45_000));

let exitCode = 0;
let browser;
try {
  if (!(await waitForHealth())) {
    console.error('wisp-agent no respondió en /health:\n' + agentLog);
    process.exit(1);
  }
  record('wisp-agent serve responde /health', true);

  const opened = await fetch(`http://127.0.0.1:${port}/tabs`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ url: fixture.url }),
  });
  record('API HTTP /tabs abre la página', opened.ok, `status ${opened.status}`);

  browser = await chromium.connectOverCDP(endpoint, { timeout: 20_000 });
  record('chromium.connectOverCDP', true, endpoint);

  const context = browser.contexts()[0];
  record('browser.contexts()[0]', Boolean(context));

  const page = context.pages()[0];
  record('context.pages()[0]', Boolean(page), `páginas: ${context.pages().length}`);

  // A partir de aquí, el ciclo de vida de página que Playwright exige.
  let navigated = false;
  try {
    await page.goto(fixture.url, { timeout: 10_000 });
    navigated = true;
    record('page.goto', true);
  } catch (error) {
    record('page.goto', false, error.message.split('\n')[0], false);
  }

  // Playwright instala un script de utilidades en cada contexto; si el motor
  // aún no soporta algún constructo, los pasos siguientes pueden quedarse
  // esperando para siempre. Cada paso va acotado en tiempo para que el test
  // informe en vez de colgarse.
  const bounded = async (name, fn, ms = 5_000) => {
    const timeout = new Promise((resolve) => setTimeout(() => resolve({ timeout: true }), ms));
    const attempt = Promise.resolve()
      .then(fn)
      .then((value) => ({ value }))
      .catch((error) => ({ error: error.message.split(String.fromCharCode(10))[0] }));
    const outcome = await Promise.race([attempt, timeout]);
    if (outcome.timeout) {
      record(name, false, `sin respuesta en ${ms} ms`, false);
      return undefined;
    }
    if (outcome.error) {
      record(name, false, outcome.error, false);
      return undefined;
    }
    return outcome.value;
  };

  if (navigated) {
    const title = await bounded('page.title()', () => page.title());
    if (title !== undefined) {
      record('page.title()', typeof title === 'string', JSON.stringify(title), false);
    }

    const text = await bounded('locator(#h).textContent()', () =>
      page.locator('#h').textContent({ timeout: 3_000 }),
    );
    if (text !== undefined) {
      record('locator(#h).textContent()', text === 'Hola', JSON.stringify(text), false);
    }

    const shot = await bounded('page.screenshot()', () =>
      page.screenshot({ type: 'png', timeout: 4_000 }),
    );
    if (shot !== undefined) {
      record('page.screenshot()', shot.length > 0 && shot[0] === 0x89, `${shot.length} bytes`, false);
    }
  } else {
    record('page.title()/locator/screenshot', false, 'omitidos: sin navegación', false);
  }
} catch (error) {
  record('conexión CDP', false, error.message.split('\n')[0]);
  exitCode = 1;
} finally {
  if (browser) {
    await Promise.race([
      browser.close().catch(() => {}),
      new Promise((resolve) => setTimeout(resolve, 3_000)),
    ]);
  }
  agent.kill();
  fixture.server.close();
  clearTimeout(watchdog);
}

if (process.env.WISP_CDP_TRACE) {
  console.log('\n--- traza del servidor ---\n' + agentLog);
}

const required = results.filter((result) => result.required);
const failedRequired = required.filter((result) => !result.ok);
const diagnostics = results.filter((result) => !result.required && !result.ok);

console.log(
  `\n${required.length - failedRequired.length}/${required.length} comprobaciones requeridas OK`
);
for (const result of results.filter((entry) => !entry.required)) {
  console.log(`  diag: ${result.ok ? 'OK  ' : 'PEND'} ${result.name}`);
}
if (diagnostics.length > 0) {
  console.log(
    '  Limitación conocida: Playwright inyecta un script de utilidades en el '
    + 'contexto y el motor aún no ejecuta todos los constructos que usa.'
  );
}
if (failedRequired.length > 0) {
  console.log('Fallos requeridos: ' + failedRequired.map((r) => r.name).join(', '));
  exitCode = 1;
}
if (process.env.WISP_CDP_TRACE) {
  console.log('  trazas CDP: WISP_CDP_TRACE=1 las imprime el servidor');
}
process.exit(exitCode);
