'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const {
  buildChecks,
  overallStatus,
  parseArguments,
  versionCheck,
} = require('../scripts/verify-delivery.cjs');

test('delivery gate covers every bridge runtime layer', () => {
  assert.deepEqual(
    buildChecks().map(check => check.id),
    [
      'bridge-node-check',
      'bridge-node-tests',
      'pxlogic-rust-format',
      'pxlogic-core-tests',
      'pxlogic-helper-check',
      'tauri-rust-format',
      'tauri-rust-tests',
    ],
  );
});

test('delivery gate fails only on a failed required check', () => {
  assert.equal(overallStatus([{ status: 'PASS' }, { status: 'WARN' }]), 'WARN');
  assert.equal(overallStatus([{ status: 'PASS' }, { status: 'FAIL' }]), 'FAIL');
  assert.equal(overallStatus([{ status: 'PASS' }]), 'PASS');
});

test('delivery report path is explicit and version drift remains visible', () => {
  const options = parseArguments(['--', '--report', './delivery-report.json']);
  assert.equal(options.reportPath, path.resolve('./delivery-report.json'));
  assert.throws(() => parseArguments(['--report']), /requires a file path/);
  const versions = versionCheck();
  assert.match(versions.status, /^(PASS|WARN)$/);
  assert.equal(typeof versions.versions.tauriConfig, 'string');
});

test('experimental profile launch keeps an explicit one-shot confirmation contract', () => {
  const rendererRoot = path.resolve(__dirname, '../client/renderer');
  const html = fs.readFileSync(path.join(rendererRoot, 'index.html'), 'utf8');
  const app = fs.readFileSync(path.join(rendererRoot, 'app.js'), 'utf8');

  assert.match(html, /id="experimental-confirmation"/);
  assert.match(html, /id="experimental-confirmation-checkbox"/);
  assert.match(html, /id="continue-experimental-button"[^>]*disabled/);
  assert.match(app, /let experimentalConfirmationToken = null;/);
  assert.match(app, /phase: currentState\.phase/);
  assert.match(app, /experimentalConfirmationToken\.confirmed = true/);
  assert.match(app, /!elements\.experimentalConfirmationCheckbox\.checked/);
  assert.match(app, /if \(elements\.experimentalConfirmation\.open\) elements\.experimentalConfirmation\.close\(\)/);
  assert.match(app, /function requestExperimentalConfirmation\(\)/);
  assert.match(app, /function consumeExperimentalConfirmationFingerprint\(\)/);
  assert.match(app, /pendingProfileFingerprint,/);
});

test('firmware picker defaults to the latest image and confirms a downgrade', () => {
  const rendererRoot = path.resolve(__dirname, '../client/renderer');
  const html = fs.readFileSync(path.join(rendererRoot, 'index.html'), 'utf8');
  const app = fs.readFileSync(path.join(rendererRoot, 'app.js'), 'utf8');

  assert.match(html, /id="pxlogic-firmware-version"/);
  assert.match(html, /id="firmware-downgrade-warning"[^>]*hidden/);
  assert.match(html, /id="firmware-downgrade-checkbox"/);
  assert.match(html, /id="confirm-firmware-downgrade-button"[^>]*disabled/);

  // An unknown or missing stored selection resolves to the image flagged latest.
  assert.match(app, /function latestFirmwareRelease\(\)/);
  assert.match(app, /findFirmwareRelease\(selectedId\) \|\| latest/);
  // Only a non-latest selection may open the confirmation dialog.
  assert.match(app, /if \(!release \|\| release\.latest \|\| release\.id === lastConfirmedFirmwareId\) return true;/);
  assert.match(app, /if \(!elements\.firmwareDowngradeCheckbox\.checked\) return;/);
  // Cancelling must restore the previous selection rather than leave the older
  // image staged for the next Bridge start.
  assert.match(app, /findFirmwareRelease\(lastConfirmedFirmwareId\) \|\| latestFirmwareRelease\(\)/);
  assert.match(app, /if \(accepted\) persistSettings\(\);/);
  assert.match(app, /pxlogicFirmwareId: elements\.pxlogicFirmwareVersion\.value,/);
});

test('every hint affordance resolves to glossary copy and stays keyboard reachable', () => {
  const rendererRoot = path.resolve(__dirname, '../client/renderer');
  const hints = fs.readFileSync(path.join(rendererRoot, 'hints.js'), 'utf8');
  const windows = ['index.html', 'status-panel.html'].map(name => ({
    name,
    html: fs.readFileSync(path.join(rendererRoot, name), 'utf8'),
  }));

  // The glossary keys are declared one per line so this static extraction can
  // stay honest; reformatting the object trips the set comparison below.
  const declared = new Set(
    [...hints.matchAll(/^ {2}'([a-z-]+)': \{$/gm)].map(match => match[1]),
  );
  assert.ok(declared.size >= 8, `expected the glossary to be populated, saw ${declared.size}`);

  const referenced = new Set();
  for (const { name, html } of windows) {
    for (const match of html.matchAll(/<button\b[^>]*\bclass="hint-button"[^>]*>/g)) {
      const tag = match[0];
      const key = tag.match(/data-hint="([a-z-]+)"/)?.[1];
      assert.ok(key, `${name}: a hint button is missing data-hint (${tag})`);
      // A `?` glyph carries no meaning for assistive technology on its own.
      assert.match(tag, /aria-label="[^"]+"/, `${name}: hint button ${key} needs an accessible name`);
      assert.match(tag, /type="button"/, `${name}: hint button ${key} must not submit`);
      referenced.add(key);
    }
    assert.match(html, /<link rel="stylesheet" href="hints\.css">/, `${name} must load hints.css`);
    assert.match(html, /<script src="hints\.js"><\/script>/, `${name} must load hints.js`);
  }

  // Both directions matter: an orphaned key is dead copy, and an unreferenced
  // key means an affordance renders an empty bubble.
  assert.deepEqual(
    [...referenced].sort(),
    [...declared].sort(),
    'every data-hint must have glossary copy and every glossary entry must be reachable',
  );

  // Dismissal and focus handling are the accessibility contract for a
  // click-triggered bubble that never receives focus itself.
  assert.match(hints, /event\.key === 'Escape'/);
  assert.match(hints, /button\.setAttribute\('aria-expanded', 'true'\)/);
  assert.match(hints, /button\.setAttribute\('aria-describedby', popover\.id\)/);
  assert.match(hints, /trigger\.focus\(\);/);
  assert.match(hints, /popover\.setAttribute\('role', 'tooltip'\)/);
});

test('the panel reveals itself once the bridge is live and explains why', () => {
  const rendererRoot = path.resolve(__dirname, '../client/renderer');
  const html = fs.readFileSync(path.join(rendererRoot, 'status-panel.html'), 'utf8');
  const script = fs.readFileSync(path.join(rendererRoot, 'status-panel.js'), 'utf8');
  const backend = fs.readFileSync(
    path.resolve(__dirname, '../tauri-client/src-tauri/src/main.rs'),
    'utf8',
  );

  assert.match(html, /id="panel-intro"[^>]*hidden/);
  assert.match(html, /id="panel-intro-dismiss"/);
  assert.match(html, /id="panel-intro-disable"/);
  // The banner has to be dismissible for good and offer a way to opt out of the
  // automatic reveal, otherwise an always-on-top window is just an intrusion.
  assert.match(script, /invoke\('status_panel_intro_acknowledge'\)/);
  assert.match(script, /invoke\('status_panel_set_auto_show', \{ enabled: false \}\)/);
  assert.match(script, /elements\.intro\.hidden = Boolean\(initial\.settings\?\.guidance\?\.statusPanelIntroSeen\)/);

  // Revealing the panel must not steal focus back from Logic 2, which is what
  // the activation-policy call in the manual path does.
  assert.match(backend, /fn show_status_panel_without_activating\(app: &AppHandle\)/);
  const automatic = backend.slice(
    backend.indexOf('fn show_status_panel_without_activating'),
  );
  const body = automatic.slice(0, automatic.indexOf('\n}\n'));
  assert.ok(
    !body.includes('set_activation_policy'),
    'the automatic reveal must not promote the activation policy',
  );
  assert.match(backend, /maybe_auto_show_status_panel\(app, &phase\);/);
});

test('the first run explains the split of responsibilities and only asks once', () => {
  const rendererRoot = path.resolve(__dirname, '../client/renderer');
  const html = fs.readFileSync(path.join(rendererRoot, 'index.html'), 'utf8');
  const app = fs.readFileSync(path.join(rendererRoot, 'app.js'), 'utf8');

  const steps = [...html.matchAll(/<section class="wizard-step" data-step="(\d)"/g)].map(
    match => Number(match[1]),
  );
  assert.deepEqual(steps, [1, 2, 3, 4], 'the walkthrough is four ordered steps');
  assert.match(html, /id="onboarding-wizard"/);
  assert.match(html, /id="onboarding-button"/);
  assert.match(html, /id="wizard-step-index"/);
  for (const control of ['wizard-skip', 'wizard-back', 'wizard-next']) {
    assert.match(html, new RegExp(`id="${control}"[^>]*type="button"`), `${control} must exist`);
  }
  assert.match(html, /id="wizard-back"[^>]*disabled/, 'step one has nowhere to go back to');

  // The single most important thing a new user needs: who owns what. Losing this
  // copy would make every other explanation harder to follow.
  assert.match(html, /Logic 2 决定/);
  assert.match(html, /Bridge 决定/);
  assert.match(html, /Demo Logic Pro 16/);

  // Every readiness cell and wizard step hands the user to a real control rather
  // than duplicating it, so the guidance can never disagree with the settings.
  const focusTargets = [...html.matchAll(/data-focus="([a-z-]+)"/g)].map(match => match[1]);
  assert.deepEqual(
    [...new Set(focusTargets)].sort(),
    ['logic-path', 'pxlogic-device', 'pxlogic-threshold'],
  );
  for (const id of new Set(focusTargets)) {
    assert.match(html, new RegExp(`id="${id}"`), `data-focus="${id}" needs a target`);
  }
  assert.match(app, /function focusSetting\(targetId\)/);
  assert.match(app, /section\.classList\.add\('section-highlight'\)/);

  // Completion and skipping both record an answer; the header button is the way
  // back in. Escape closes a <dialog> natively and bypasses every button, so the
  // record has to hang off the close event or the walkthrough silently returns on
  // the next launch.
  assert.match(app, /invoke\('onboarding_complete'\)/);
  assert.match(app, /elements\.wizard\.addEventListener\('close', \(\) => void recordOnboardingComplete\(\)\)/);
  assert.match(app, /elements\.wizardSkip\.addEventListener\('click', \(\) => finishWizard\(\)\)/);
  assert.match(app, /if \(Number\(guidance\.onboardingCompletedVersion \|\| 0\) > 0\) return;/);
  // A host without persisted guidance must not be nagged on every launch.
  assert.match(app, /if \(!guidance \|\| typeof api\.completeOnboarding !== 'function'\) \{/);
});

test('renderer settings can never reach disk without the backend-owned merge', () => {
  const backend = fs.readFileSync(
    path.resolve(__dirname, '../tauri-client/src-tauri/src/main.rs'),
    'utf8',
  );

  // Every function that accepts a ClientSettings from the UI must funnel through
  // the merging helper. Writing renderer settings directly resets the panel
  // geometry and the walkthrough flags, which the renderer never sends back.
  assert.match(backend, /fn store_renderer_settings\(/);
  assert.match(backend, /fn client_save_settings\([\s\S]*?store_renderer_settings\(&app, settings\)/);
  assert.match(
    backend,
    /fn start_bridge_inner\(app: &AppHandle, settings: ClientSettings\)[\s\S]{0,240}?store_renderer_settings\(app, settings\)/,
  );

  // Any other entry point taking renderer settings would need the same treatment,
  // so fail loudly when one appears.
  const entryPoints = [...backend.matchAll(/fn (\w+)\([^)]*settings: ClientSettings[^)]*\)/g)]
    .map(match => match[1])
    .filter(name => !['merge_backend_owned_settings', 'store_settings', 'store_renderer_settings'].includes(name));
  assert.deepEqual(
    entryPoints.sort(),
    ['bridge_restart', 'bridge_start', 'client_save_settings', 'start_bridge_inner'],
    'a new settings entry point must route through store_renderer_settings',
  );
});

test('the status panel draws its own chrome and packs the readout tightly', () => {
  const rendererRoot = path.resolve(__dirname, '../client/renderer');
  const html = fs.readFileSync(path.join(rendererRoot, 'status-panel.html'), 'utf8');
  const css = fs.readFileSync(path.join(rendererRoot, 'status-panel.css'), 'utf8');
  const script = fs.readFileSync(path.join(rendererRoot, 'status-panel.js'), 'utf8');
  const backend = fs.readFileSync(
    path.resolve(__dirname, '../tauri-client/src-tauri/src/main.rs'),
    'utf8',
  );

  // The collapsed shape must be one button covering the whole window. A small
  // icon inside a bar was the previous design and it was too fiddly to hit,
  // which defeated the point of collapsing at all.
  const chip = html.match(/<button id="panel-chip"[^>]*>/)?.[0];
  assert.ok(chip, 'the collapsed shape needs to be a button');
  assert.match(chip, /type="button"/);
  assert.match(chip, /title="[^"]+"/);
  assert.match(html, /id="chip-dot"/);
  assert.match(html, /id="chip-label"/);
  assert.match(css, /\.panel-chip \{/);
  assert.match(css, /body\.collapsed \.panel-chip \{\n\s*position: fixed;\n\s*display: flex;\n\s*inset: 0;\n\s*\}/);
  for (const hidden of ['.panel-header', 'main', 'footer']) {
    assert.ok(
      css.includes(`body.collapsed ${hidden}`),
      `${hidden} must be hidden while collapsed`,
    );
  }

  // The header carries the live state and the device identity together, because
  // "am I connected" and "to what" are one question. A separate title line and a
  // separate device section were pure chrome.
  assert.match(html, /<div class="live-state">[\s\S]*?id="state-dot"[\s\S]*?id="state-label"[\s\S]*?id="state-detail"/);
  assert.match(html, /<div class="live-device">[\s\S]*?id="device-label"[\s\S]*?id="device-serial"/);
  assert.ok(!html.includes('class="eyebrow"'), 'the panel must not spend a line on branding');
  assert.ok(!/<h1>/.test(html), 'a static title is not worth a line in a 340 px panel');

  // The data link is two tight lines in one card rather than a section of its own.
  assert.match(html, /<section class="quality-row">/);
  assert.match(css, /\.quality-row \{/);

  // Expanding hangs off the button's own click event so keyboard activation and
  // assistive technology work; the pointer tracking only hands a real drag to the
  // window manager and suppresses the click it ends with. Stray pointer moves
  // around window activation must not be mistaken for a drag.
  assert.match(script, /const DRAG_THRESHOLD = 6;/);
  assert.match(script, /if \(!\(event\.buttons & 1\)\) return;/);
  assert.match(script, /if \(control && control !== handle\) return;/);
  assert.match(script, /invoke\('status_panel_start_drag'\)/);
  // Both shapes must be movable: with no titlebar the header is the drag handle.
  assert.match(script, /bindDragHandle\(elements\.chip, \(\) => void setCollapsed\(false\)\)/);
  assert.match(script, /bindDragHandle\(elements\.header, null\)/);
  assert.match(script, /invoke\('status_panel_set_collapsed', \{ collapsed \}\)/);
  assert.match(script, /applyCollapsed\(initial\.settings\?\.statusPanel\?\.collapsed\)/);

  assert.match(backend, /fn status_panel_start_drag\(app: AppHandle\)/);
  // Cocoa anchors a resize at the bottom-left, so the shape change has to put the
  // origin back or the chip slides away from where the panel was.
  assert.match(backend, /let anchor = window\.outer_position\(\)\.ok\(\);/);
  assert.match(backend, /if let Some\(anchor\) = anchor \{\n\s*let _ = window\.set_position\(anchor\);/);
  // Toggling decorations is gone: the window never has them.
  assert.ok(
    !backend.includes('set_decorations'),
    'the panel window is undecorated in both shapes',
  );
  // Changing shape can detach an edge-snapped panel, so re-settle afterwards.
  assert.match(backend, /settle_status_panel\(&app, &window\);/);
});

test('the native host is signed so copies of it survive macOS gatekeeping', () => {
  const bridgeRoot = path.resolve(__dirname, '..');
  const prepare = fs.readFileSync(
    path.join(bridgeRoot, 'client/scripts/prepare-payload.cjs'),
    'utf8',
  );
  const index = fs.readFileSync(path.join(bridgeRoot, 'index.cjs'), 'utf8');

  // clang and rustc emit "linker-signed" ad-hoc signatures. Those stop
  // validating once the binary is copied by a process carrying provenance, which
  // is exactly what Tauri's resource staging does, and macOS then kills the copy
  // with SIGKILL before it prints anything. Re-signing is the fix; losing it
  // reintroduces a failure that only shows up as "GraphServer exited before
  // ready".
  assert.match(prepare, /function resignForMacos\(file\)/);
  assert.match(prepare, /run\('codesign', \['--force', '--sign', '-', file\]\)/);
  assert.match(prepare, /resignForMacos\(helperDestination\);/);
  assert.match(prepare, /resignForMacos\(nativeDestination\);/);
  assert.match(prepare, /resignForMacos\(bridgeBuildHost\);/);
  // The on-demand compile in the Bridge runtime produces the same kind of
  // signature and needs the same treatment.
  assert.match(index, /'codesign',\n\s*\['--force', '--sign', '-', executable\]/);

  // Staging must prove the binary can start, otherwise a bad signature stays
  // invisible until the Bridge fails at runtime.
  assert.match(prepare, /if \(usage\.signal\) \{/);
  assert.match(prepare, /the code signature is most likely invalid/);
});

test('every selectable firmware image is shipped and matches the manifest', () => {
  const firmwareRoot = path.resolve(__dirname, '../../../resources/firmware');
  const manifest = JSON.parse(fs.readFileSync(path.join(firmwareRoot, 'releases.json'), 'utf8'));

  assert.equal(manifest.schemaVersion, 1);
  const latest = manifest.releases.filter(release => release.latest);
  assert.equal(latest.length, 1, 'exactly one image may be marked latest');
  assert.equal(latest[0].id, manifest.default, 'the default selection must be the latest image');

  const crypto = require('node:crypto');
  for (const release of manifest.releases) {
    const image = fs.readFileSync(path.join(firmwareRoot, release.fileName));
    assert.equal(image.length, release.byteLength, `${release.fileName} length`);
    assert.equal(
      crypto.createHash('sha256').update(image).digest('hex'),
      release.sha256,
      `${release.fileName} digest`,
    );
  }
});
