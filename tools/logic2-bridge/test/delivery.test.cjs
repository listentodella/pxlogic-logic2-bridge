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
  const index = fs.readFileSync(path.resolve(__dirname, '../index.cjs'), 'utf8');
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
  assert.match(html, /<div class="live-state">[\s\S]*?id="state-dot"[\s\S]*?id="state-label"[\s\S]*?<\/div>/);
  assert.match(html, /<div class="live-device">[\s\S]*?id="device-label"[\s\S]*?id="device-serial"/);
  assert.ok(!html.includes('class="eyebrow"'), 'the panel must not spend a line on branding');
  assert.ok(!/<h1>/.test(html), 'a static title is not worth a line in a 340 px panel');

  // The state label already reads 已连接 or 未连接, so nothing else shares its row.
  // The line below it exists only for what the label cannot say -- an error code
  // or a failed command -- and starts hidden instead of restating the label.
  const liveState = html.match(/<div class="live-state">[\s\S]*?<\/div>/)[0];
  assert.ok(
    !liveState.includes('state-detail'),
    'the state row must not carry small print alongside the label',
  );
  assert.match(html, /<p id="state-detail" class="live-problem" hidden><\/p>/);
  assert.ok(
    !html.includes('Bridge 已连接到 Logic 2'),
    'the state label already reports whether the Bridge reached Logic 2',
  );
  assert.match(script, /function showProblem\(/);
  assert.match(script, /elements\.detail\.hidden = !message;/);

  // A 24-character serial cannot share a row with a model name without one of
  // them being cut short, so the identity lines stack.
  assert.match(css, /\.live-device \{[\s\S]*?flex-direction: column;/);

  // Enabled channels are a map, not a sentence. "D0, D1, D2, D3" fitted the cell but
  // the sixteen channels Logic 2 can offer never will, so the answer was truncated
  // exactly when it got interesting.
  assert.match(html, /<div id="channel-grid" class="channel-grid"\s*\n?\s*role="img"/);
  assert.ok(
    !html.includes('id="channels"'),
    'the channel readout is a grid now, not a comma-separated string',
  );
  // Logic 2's own palette, so a channel is the same colour here as in the waveform.
  assert.match(script, /const CHANNEL_COLORS = \[\n\s*'#d4d4d4', '#C79579', '#FF6D7F', '#FFB45B',\n\s*'#e8d836', '#58c667', '#53A9FD', '#AF92FB',\n\s*\];/);
  // Sixteen is a floor, not a size: a capture reporting a higher index must widen the
  // grid rather than omit the channel.
  assert.match(script, /const CHANNEL_GRID_MINIMUM = 16;/);
  assert.match(script, /Math\.ceil\(needed \/ CHANNEL_GRID_COLUMNS\) \* CHANNEL_GRID_COLUMNS/);
  // Logic 2 owns channel selection, so the cells must not be pressable.
  assert.match(script, /document\.createElement\('span'\)/);
  assert.ok(
    !/channel-cell[^]*?<button/.test(html),
    'channel cells report state and must not look like controls',
  );
  // The palette is built for Logic 2's dark theme; #d4d4d4 is invisible on white.
  assert.match(css, /\.channel-grid \{[\s\S]*?background: #22252a;/);
  // A picture needs its reading spelled out.
  assert.match(script, /setAttribute\(\n\s*'aria-label',/);

  // The comparator threshold is the one value the panel can change. It was a launch
  // argument only, so a wrong guess could only be corrected by closing Logic 2 and
  // losing the capture in it, for a value that can only be judged from the result.
  assert.match(html, /<input id="threshold"\s*\n?\s*type="number" step="0\.05" min="0" max="6\.668"/);
  assert.match(script, /invoke\('status_panel_set_threshold', \{ volts \}\)/);
  // `change` fires on blur and Enter, so a half-typed number never reaches hardware.
  assert.match(script, /elements\.threshold\.addEventListener\('change'/);
  // The helper is handed the threshold when it arms, so mid-capture edits could only
  // ever half-apply.
  assert.match(script, /setThresholdEditable\(\['starting', 'streaming'\]\.includes/);
  assert.match(backend, /fn status_panel_set_threshold\(app: AppHandle, volts: f64\)/);
  // The session had no inbound channel at all, which is why a setting could only be
  // changed by restarting it. Only the framing lives in the entry point.
  assert.match(backend, /control: Option<ChildStdin>,/);
  assert.match(backend, /let control = child\.stdin\.take\(\);/);
  assert.match(index, /function startControlChannel\(controller, markerService\)/);
  assert.match(index, /applyBridgeControlCommand\(controller, line\)/);
  assert.match(backend, /"采集进行中，请先在 Logic 2 里停止采集"/);
  // A rejected edit must not leave the field claiming a threshold that is not in force.
  assert.match(script, /renderThreshold\(appliedThreshold\);\n\s*showProblem\(/);
  // Either window can retune it, and each used to read the value once at load and never
  // hear about the other's change -- with the stale copy in the main window's form
  // silently putting the old threshold back on its next save. Every settings write
  // funnels through `store_settings`, so that is where the two are kept in step.
  assert.match(backend, /"pxlogic-threshold",\n\s*PxlogicThresholdChange \{/);
  assert.match(script, /listen\('pxlogic-threshold', event => renderThreshold\(event\.payload\?\.volts\)\)/);
  const mainScript = fs.readFileSync(path.join(rendererRoot, 'app.js'), 'utf8');
  assert.match(mainScript, /onThreshold: callback => void listen\('pxlogic-threshold'/);
  assert.match(mainScript, /if \(document\.activeElement === elements\.pxlogicThreshold\) return;/);

  // Stopping the Bridge closes Logic 2 and takes any unsaved capture with it, and the
  // panel had no way to do it at all: the user had to go back to the main window. It
  // arms first rather than acting on one click, because this window floats over Logic 2.
  assert.match(html, /<button id="stop-button" class="stop-button" type="button" hidden>停止 Bridge<\/button>/);
  assert.match(script, /invoke\('bridge_stop'\)/);
  assert.match(script, /if \(!elements\.stop\.classList\.contains\('armed'\)\) \{\n\s*armStop\(\);/);
  assert.match(script, /未保存的采集数据将会丢失/);
  // A session that ended must not leave an armed button pointed at the next one.
  assert.match(script, /elements\.stop\.hidden = !live;\n\s*if \(!live\) disarmStop\(\);/);
  // Shutting a session down closes Logic 2 and the capture helper with it, which does not
  // always finish inside the grace period; the kill that ends it must not be reported as
  // a fault.
  assert.match(backend, /stop_requested: bool,/);
  assert.match(backend, /runtime\.stop_requested = true;/);
  assert.match(backend, /runtime\.stop_requested = false;/);
  assert.match(backend, /\.map\(\|runtime\| runtime\.stop_requested\)/);
  assert.match(backend, /const BRIDGE_STOP_GRACE: Duration = Duration::from_secs\(10\);/);

  // Logic 2 lowers its own rate when the enabled channels make the request impossible and
  // does not tell the GraphServer, so the last rate it sent can contradict its own UI. The
  // derived clamp is displayed instead, with the request kept in the tooltip.
  assert.match(script, /function renderRates\(values\)/);
  assert.match(script, /elements\.logicRate\.textContent = formatRate\(inForce\);/);
  assert.match(script, /elements\.logicRate\.classList\.toggle\('reduced', reduced\)/);
  assert.match(css, /\.metric strong\.reduced::after \{[\s\S]*?content: "已降频";/);

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
  assert.match(script, /invoke\('status_panel_begin_move'\)/);
  assert.match(script, /invoke\('status_panel_move'\)/);
  assert.match(script, /invoke\('status_panel_end_move'\)/);
  // Both shapes must be movable: with no titlebar the header is the drag handle.
  assert.match(script, /bindDragHandle\(elements\.chip, \(\) => void setCollapsed\(false\)\)/);
  assert.match(script, /bindDragHandle\(elements\.header, null\)/);
  assert.match(script, /invoke\('status_panel_set_collapsed', \{ collapsed \}\)/);
  assert.match(script, /applyCollapsed\(initial\.settings\?\.statusPanel\?\.collapsed\)/);

  assert.match(backend, /fn status_panel_begin_move\(app: AppHandle\)/);
  // Handing the gesture to the window manager is what arms macOS edge tiling, so
  // the panel is moved from the cursor's own displacement instead and held on the
  // work area for the whole drag.
  assert.ok(
    !/\.start_dragging\(\)/.test(backend),
    'a native window drag lets macOS offer to tile the panel at a screen edge',
  );
  assert.match(backend, /fn confine_dragged_panel\(/);
  assert.match(backend, /let \(x, y\) = confine_dragged_panel\(moved, cursor, &panel_work_areas\(&window\)\);/);
  assert.match(backend, /x: anchor\.window_x \+ \(cursor\.x - anchor\.cursor_x\)\.round\(\) as i32,/);

  // Docked on the bottom edge the panel mirrors itself, so the drag handle and the
  // collapse control stay on the edge a collapsing chip will rest on. The Bridge
  // owns the decision because it owns the work-area geometry and the tolerance.
  assert.match(css, /body \{[\s\S]*?display: flex;[\s\S]*?flex-direction: column;/);
  assert.match(css, /body\.dock-bottom \.panel-header \{[\s\S]*?order: 1;/);
  assert.match(css, /body\.dock-bottom \.panel-header \{[\s\S]*?border-top: 1px solid #151619;/);
  assert.match(script, /listen\('status-panel-dock', event => applyDock\(event\.payload\?\.bottom\)\)/);
  assert.match(script, /classList\.toggle\('dock-bottom', Boolean\(bottom\)\)/);
  // A reload can land long after the last move, and the change event will not fire.
  assert.match(script, /invoke\('status_panel_dock_edge'\)\.then\(applyDock\)/);
  assert.match(backend, /fn panel_docked_at_bottom\(/);
  assert.match(backend, /STATUS_PANEL_SNAP_THRESHOLD\)\n\}/);
  // Every path that can change the panel's geometry has to resolve the layout.
  const dockSyncs = backend.match(/sync_status_panel_dock\(/g) || [];
  assert.ok(
    dockSyncs.length >= 5,
    `the dock state must be resolved after a resize, a drag, a settle and both reveals, saw ${dockSyncs.length} call sites`,
  );

  // Nothing scrolls. The readout is short and collapses into a chip when it is in
  // the way, so the window follows the content instead of offering a scrollbar. No
  // constant can predict the height: the first-run card and an error line each add a
  // chunk, so the renderer measures and the Bridge applies.
  assert.match(css, /html, body \{ overflow: hidden; \}/);
  assert.match(script, /invoke\('status_panel_fit_height', \{ height: Math\.ceil\(height\) \}\)/);
  assert.match(script, /new ResizeObserver\(\(\) => fitToContent\(\)\)/);
  assert.match(backend, /fn status_panel_fit_height\(app: AppHandle, height: f64\)/);
  // Locking the height is what makes a scrollbar impossible rather than merely
  // hidden, and the lock has to be released before a 44 px chip can fit.
  assert.match(backend, /set_max_size\(Some\(tauri::LogicalSize::new\(\n\s*STATUS_PANEL_MAX_WIDTH,/);
  assert.match(backend, /set_max_size\(None::<tauri::LogicalSize<f64>>\)/);
  // Without a tolerance the physical/logical rounding bounces against the observer.
  assert.match(backend, /if \(physical - rect\.height\)\.abs\(\) <= 1 \{/);

  // Expanding must not be visible as two steps. Each window mutation posted on its
  // own can be committed as its own frame, so the panel gets drawn at the new size in
  // the old place first; they go in one main-thread turn instead.
  assert.match(backend, /fn apply_panel_frame\(/);
  assert.match(backend, /run_on_main_thread\(apply\)\.is_ok\(\)/);
  assert.ok(
    !/\n    let _ = window\.set_size\(tauri::LogicalSize::new\(width, height\)\);/.test(backend),
    'shape changes must go through apply_panel_frame rather than resizing directly',
  );
  // The orientation is decided from where the chip will expand to, not from the chip:
  // a chip just clear of the bottom edge expands flush against it, and settling that
  // afterwards makes the header visibly jump across the panel.
  assert.match(backend, /fn project_expanded_panel\(/);
  assert.match(backend, /fn publish_dock_for_panel\(/);
  assert.match(
    backend,
    /publish_dock_for_panel\(app, scale, target, &work_areas\);\n\s*let floor =/,
  );
  // Opening at the height the content measured last time removes the second resize.
  assert.match(backend, /expanded_panel_height\s*\n?\s*\.store\(target\.round\(\) as u32/);
  // Cocoa anchors a resize at the bottom-left, so the shape change has to put the
  // origin back or the chip slides away from where the panel was. The chip can be
  // parked anywhere, including hard against an edge, so the new shape is then
  // placed rather than merely restored: growing in place would push the readout
  // off the display, and the snap and clamp rules both let that through.
  assert.match(backend, /let Some\(anchor\) = panel_rect\(window\) else \{/);
  assert.match(backend, /fn place_resized_panel\(/);
  assert.match(backend, /fn anchored_axis\(/);
  assert.match(
    backend,
    /let \(x, y\) = place_resized_panel\(anchor, resized, &work_areas\);/,
  );
  // The size has to come from the request, not from a read-back that can still
  // report the shape being left behind.
  assert.match(backend, /width: \(width \* scale\)\.round\(\) as i32,/);
  assert.match(backend, /height: \(height \* scale\)\.round\(\) as i32,/);
  assert.match(backend, /let _ = window\.set_position\(tauri::PhysicalPosition::new\(x, y\)\);/);
  // Toggling decorations is gone: the window never has them.
  assert.ok(
    !backend.includes('set_decorations'),
    'the panel window is undecorated in both shapes',
  );
  // A settle pass reads the size back, so it cannot run inline with the resize.
  // The debounced one behind the move event still snaps and persists the result.
  assert.ok(
    !/apply_status_panel_collapsed\(&app, &window, collapsed\);\n\s*\/\/[\s\S]{0,200}?settle_status_panel\(&app, &window\);/.test(
      backend,
    ),
    'placing the new shape must not be followed by a settle on a stale size',
  );
  assert.match(backend, /schedule_status_panel_settle\(&status\);/);
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

test('a running Logic window blocks the start instead of being reattached', () => {
  const bridgeRoot = path.resolve(__dirname, '..');
  const index = fs.readFileSync(path.join(bridgeRoot, 'index.cjs'), 'utf8');
  const proxy = fs.readFileSync(path.join(bridgeRoot, 'lib/websocket-proxy.cjs'), 'utf8');

  // Logic reconnects to the address it was given but only re-sends the
  // calibration storage root; it never recreates its session, re-acquires the
  // device, or re-applies channels and sample rate. A reattached window therefore
  // looks connected while capture silently does nothing, so every running window
  // has to be replaced rather than reused.
  assert.match(index, /function findRunningLogicInstances\(executable\)/);
  assert.match(index, /if \(running\.length\) \{/);
  assert.match(index, /LOGIC_ALREADY_RUNNING/);
  assert.match(index, /采集会静默失效/);
  // The Bridge runs through the same Logic binary in Node mode and Chromium
  // spawns helpers from it, so neither may be mistaken for a window.
  assert.match(index, /if \(pid === process\.pid \|\| !command\.startsWith\(executable\)\) continue;/);
  assert.match(index, /if \(\/\^\\S\*\\\.cjs\(\\s\|\$\)\/\.test\(args\)\) continue;/);
  assert.match(index, /--type=/);

  // Reattachment machinery must be gone, not merely unused.
  for (const orphan of ['adopted', 'waitForExternalExit', 'requireExactPort']) {
    assert.ok(!index.includes(orphan), `index.cjs still references ${orphan}`);
  }
  assert.ok(!proxy.includes('requireExactPort'), 'the proxy still takes requireExactPort');
  // Falling back to an automatic port is correct again now that no window is
  // waiting on a fixed one.
  assert.match(proxy, /if \(error\.code === 'EADDRINUSE' && requestedPort !== 0 && !retriedWithAutomaticPort\)/);
});

test('a Logic request cannot silence the session it was sent through', () => {
  const bridgeRoot = path.resolve(__dirname, '..');
  const proxy = fs.readFileSync(path.join(bridgeRoot, 'lib/websocket-proxy.cjs'), 'utf8');
  const controller = fs.readFileSync(path.join(bridgeRoot, 'lib/capture-controller.cjs'), 'utf8');

  // The observer runs before the message is relayed so that a StartCapture arriving
  // behind a channel change is served with the new configuration. That makes it the one
  // thing able to take the connection down: a rejection left the forwarding chain
  // permanently rejected, every later frame queued behind it with `.then` never ran, and
  // the proxy went on reading from Logic 2 while forwarding nothing -- silently. Logic
  // 2's controls appeared dead, because its buttons only move once the GraphServer
  // confirms the change.
  assert.match(proxy, /const transformed = await withTimeout\(/);
  assert.match(proxy, /observer failed, relaying message unchanged/);
  assert.match(proxy, /observer did not settle within/);

  // Clearing every digital channel is one click of Logic 2's Clear button. There is no
  // stream mode for zero lanes, and resolving one throws.
  assert.match(controller, /if \(settings\.enabledChannels\.length === 0\) \{[\s\S]*?reason: '未启用任何数字通道',/);

  // `done` is what the controller awaits when it tears a capture down, so it has to
  // settle on every path; it used to be resolved only after an unbounded post-mortem
  // scan, and a stall there left the observer waiting forever.
  assert.match(controller, /\} finally \{\n\s*resolveDone\(\{ code, failure \}\);/);
  assert.match(controller, /PXLogic stop before start failed/);
});

test('the launcher confirms before closing a running Logic window', () => {
  const rendererRoot = path.resolve(__dirname, '../client/renderer');
  const html = fs.readFileSync(path.join(rendererRoot, 'index.html'), 'utf8');
  const app = fs.readFileSync(path.join(rendererRoot, 'app.js'), 'utf8');
  const backend = fs.readFileSync(
    path.resolve(__dirname, '../tauri-client/src-tauri/src/main.rs'),
    'utf8',
  );

  assert.match(html, /id="logic-running-confirmation"/);
  assert.match(html, /id="logic-running-checkbox"/);
  assert.match(html, /id="confirm-logic-running-button"[^>]*disabled/);
  // Closing someone else's application can lose unsaved captures, so the risk has
  // to be stated and the checkbox has to gate the button.
  assert.match(html, /未保存的采集数据[\s\S]{0,40}将会丢失/);
  assert.match(html, /我已保存需要保留的采集数据/);
  assert.match(html, /点击采集不会有任何反应/);
  assert.match(app, /if \(!elements\.logicRunningCheckbox\.checked\) return;/);
  assert.match(app, /elements\.confirmLogicRunningButton\.disabled = !elements\.logicRunningCheckbox\.checked/);
  // Escape closes a <dialog> natively and must count as declining.
  assert.match(app, /const onClose = \(\) => settle\(false\);/);
  // The check runs before start, and a decline aborts it.
  assert.match(app, /if \(!await resolveRunningLogic\(\)\) return;/);
  // Every running window is offered for closing; none can be reattached.
  assert.match(app, /const blocking = instances \|\| \[\];/);

  assert.match(backend, /fn running_logic_instances\(executable: &Path\)/);
  assert.match(backend, /fn logic_running_instances\(app: AppHandle\)/);
  assert.match(backend, /async fn logic_close_instances\(app: AppHandle, pids: Vec<u32>\)/);
  // A stale pid list from the renderer must not become a kill of anything else.
  assert.match(backend, /let known: HashSet<u32> = running_logic_instances/);
  assert.match(backend, /\.filter\(\|pid\| known\.contains\(pid\)\)/);
  // Polite first, forcible only after waiting.
  assert.match(backend, /terminate_single_process\(\*pid, false\)\?;/);
  assert.match(backend, /terminate_single_process\(\*pid, true\)/);
});

test('the native host recovers from a signature macOS has already rejected', () => {
  const bridgeRoot = path.resolve(__dirname, '..');
  const index = fs.readFileSync(path.join(bridgeRoot, 'index.cjs'), 'utf8');

  // macOS caches a rejected signature against the inode, so rewriting the file in
  // place keeps the rejection - which is exactly what Tauri's resource staging
  // does. The C source is not part of the payload, so recompiling is unavailable
  // and recovery has to work from the binary alone: a copy at a fresh path gets a
  // fresh inode, and putting it outside the installation leaves a signed
  // application bundle untouched.
  assert.match(index, /function nativeHostLaunchFailure\(executable\)/);
  assert.match(index, /if \(probe\.signal\) return `killed by \$\{probe\.signal\} on launch`;/);
  assert.match(index, /function repairNativeHost\(executable\)/);
  assert.match(index, /const scratchRoot = path\.join\(bridgeStateRoot\(\), 'native'\);/);
  assert.match(index, /fs\.rmSync\(repaired, \{ force: true \}\);/);
  assert.match(index, /return repaired;/);
  assert.match(index, /the repaired \` \+\n\s*\`copy failed too/);
  // Recompiling stays available where the source does ship, but only as a fallback.
  assert.match(index, /if \(sourceExists\) \{/);

  // The host outlives a killed launcher unless it is forced down, and an orphan
  // keeps its port and its injected hooks for the rest of the login session. Both
  // the installed path and the repaired copy can be left behind.
  assert.match(index, /function reapOrphanedNativeHosts\(executables\)/);
  assert.match(index, /if \(Number\(rawParent\) !== 1\) continue;/);
  assert.match(index, /reapOrphanedNativeHosts\(\[/);
  assert.match(index, /host\.kill\('SIGKILL'\)/);
});

test('an abandoned Bridge session is cleared before a new one starts', () => {
  const bridgeRoot = path.resolve(__dirname, '..');
  const index = fs.readFileSync(path.join(bridgeRoot, 'index.cjs'), 'utf8');

  // A session whose launcher is gone keeps its proxy port and its native host.
  // Leaving it would let two live sessions split one Logic window between them,
  // which looks like "connected but capture does nothing".
  assert.match(index, /async function reapOrphanedBridgeSessions\(logicExecutable, entryScript\)/);
  assert.match(index, /if \(pid === process\.pid \|\| Number\(rawParent\) !== 1\) continue;/);
  assert.match(index, /if \(path\.basename\(first\) !== script\) continue;/);
  // SIGTERM would run its shutdown path and close the Logic window mid-check;
  // closing it is the launcher's decision, taken with the user's confirmation.
  assert.match(index, /process\.kill\(pid, 'SIGKILL'\)/);
  // Binding races the old owner unless the wait is explicit.
  assert.match(index, /if \(!reaped\.some\(alive\)\) return;/);
  // Must run before the Logic scan so the scan sees the settled process list.
  const reapAt = index.indexOf('await reapOrphanedBridgeSessions(');
  const scanAt = index.indexOf('const running = findRunningLogicInstances(');
  assert.ok(reapAt > 0 && scanAt > reapAt, 'sessions are reaped before Logic is scanned');

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

test('Logic 2 MCP proxy keeps its independent window and safety gate', () => {
  const bridgeRoot = path.resolve(__dirname, '..');
  const rendererRoot = path.join(bridgeRoot, 'client/renderer');
  const tauriRoot = path.join(bridgeRoot, 'tauri-client/src-tauri');
  const config = JSON.parse(fs.readFileSync(path.join(tauriRoot, 'tauri.conf.json'), 'utf8'));
  const capability = JSON.parse(
    fs.readFileSync(path.join(tauriRoot, 'capabilities/default.json'), 'utf8'),
  );
  const backend = fs.readFileSync(path.join(tauriRoot, 'src/main.rs'), 'utf8');
  const proxy = fs.readFileSync(path.join(tauriRoot, 'src/mcp_proxy.rs'), 'utf8');
  const index = fs.readFileSync(path.join(rendererRoot, 'index.html'), 'utf8');
  const panel = fs.readFileSync(path.join(rendererRoot, 'mcp-panel.html'), 'utf8');
  const script = fs.readFileSync(path.join(rendererRoot, 'mcp-panel.js'), 'utf8');
  const packageJson = fs.readFileSync(path.join(bridgeRoot, 'package.json'), 'utf8');

  const window = config.app.windows.find(candidate => candidate.label === 'mcp');
  assert.ok(window, 'the MCP activity window must be packaged');
  assert.equal(window.url, 'mcp-panel.html');
  assert.equal(window.visible, false);
  assert.equal(window.alwaysOnTop, true);
  assert.equal(window.decorations, false);
  assert.equal(window.resizable, true);
  assert.equal(window.focus, false);
  assert.ok(capability.windows.includes('mcp'));
  assert.match(index, /id="mcp-panel-button"/);
  assert.match(packageJson, /node --check client\/renderer\/mcp-panel\.js/);

  // The proxy is application-scoped and starts in setup_app before the ordinary
  // tray setup; it must not depend on a Bridge capture child process.
  assert.match(backend, /fn setup_app[\s\S]*?start_mcp_proxy[\s\S]*?setup_tray/);
  assert.match(backend, /DEFAULT_LISTEN_PORT/);
  assert.match(proxy, /Ipv4Addr::LOCALHOST/);
  assert.match(proxy, /Method::POST/);
  assert.match(proxy, /Method::DELETE/);
  assert.match(proxy, /text\/event-stream/);
  assert.match(proxy, /mcp-session-id/);
  assert.match(proxy, /reject_foreign_origin/);
  assert.match(proxy, /MAX_OBSERVED_MESSAGE_BYTES/);

  // The window exposes a generic transport URL and the real catalogue/activity,
  // not a product-specific agent integration.
  for (const id of [
    'proxy-endpoint', 'fallback-warning', 'upstream-status', 'registration-value',
    'tool-list', 'activity-list', 'approval-list',
  ]) {
    assert.match(panel, new RegExp(`id="${id}"`), `${id} must remain visible to the user`);
  }
  assert.match(panel, /Streamable HTTP/);
  assert.match(script, /listen\('mcp-activity'/);
  assert.match(script, /listen\('mcp-tools'/);
  assert.match(script, /listen\('mcp-approval'/);
  assert.match(script, /invoke\('mcp_approval_resolve'/);
  assert.match(script, /本次 MCP 会话内该工具免问/);

  // Capture lifecycle and every unknown future tool stop at the gate. The fixed
  // timeout and matched JSON-RPC error keep an unattended call from hanging.
  for (const tool of ['start_capture', 'load_capture', 'stop_capture', 'close_capture']) {
    assert.match(backend, new RegExp(`"${tool}"`));
  }
  assert.match(backend, /尚未分类的 Logic 2 MCP 工具/);
  assert.match(backend, /tokio::time::timeout\(Duration::from_secs\(30\), receiver\)/);
  assert.match(proxy, /"code": -32000/);
  assert.match(backend, /approvals\.close_session\(session_id\)/);

  const trayStart = backend.indexOf('fn setup_tray');
  const trayEnd = backend.indexOf('\nfn main()', trayStart);
  const tray = backend.slice(trayStart, trayEnd);
  assert.ok(!tray.includes('MCP'), 'the existing tray menu must not gain an MCP entry');
});

test('timing markers reach the renderer without exposing a debugging surface', () => {
  const bridgeRoot = path.resolve(__dirname, '..');
  const tauriRoot = path.join(bridgeRoot, 'tauri-client/src-tauri');
  const backend = fs.readFileSync(path.join(tauriRoot, 'src/main.rs'), 'utf8');
  const proxy = fs.readFileSync(path.join(tauriRoot, 'src/mcp_proxy.rs'), 'utf8');
  const index = fs.readFileSync(path.join(bridgeRoot, 'index.cjs'), 'utf8');
  const rendererBridge = fs.readFileSync(path.join(bridgeRoot, 'lib/renderer-bridge.cjs'), 'utf8');
  const markers = fs.readFileSync(path.join(bridgeRoot, 'lib/renderer-markers.cjs'), 'utf8');

  // The debugging port is a transport for the marker tools. Nothing may turn it into a
  // visible inspector: the DevTools window is a development affordance and the user
  // asked for it to stay out of sight.
  // Matched in argument position rather than anywhere in the file: the comment above
  // the port allocation names these flags precisely to record that they are not passed,
  // and an assertion that forbids naming them would delete its own explanation.
  const passesFlag = (source, flag) =>
    new RegExp(String.raw`(?:arg|push|args)\s*(?:\(|\[)[^)\]]*${flag}`).test(source);
  for (const source of [backend, index]) {
    assert.ok(
      !passesFlag(source, 'auto-open-devtools'),
      'the DevTools window must never be opened for the user',
    );
    assert.ok(
      !passesFlag(source, 'enable-automation'),
      'the automation banner belongs to a flag this must not pass',
    );
  }
  // Again in call position: `Page.inspect` is the one CDP method that raises Logic 2's
  // own inspector, and it is named in a comment for the same reason.
  assert.ok(
    !/send\(\s*'Page\.inspect'|send\(\s*"Page\.inspect"/.test(rendererBridge),
    'Page.inspect would raise Logic 2 own inspector',
  );
  // No CDP domain is enabled, so being connected costs the renderer nothing else.
  assert.ok(
    !/Runtime\.enable|Page\.enable|DOM\.enable/.test(rendererBridge),
    'no CDP domain should be enabled',
  );

  // The port comes from the OS and only ever binds loopback.
  assert.match(backend, /fn allocate_renderer_debug_port\(\)/);
  assert.match(backend, /TcpListener::bind\(\(Ipv4Addr::LOCALHOST, 0\)\)/);
  assert.match(backend, /"--remote-debugging-port", &debug_port\.to_string\(\)/);

  // Markers are served by this client, added to whatever Logic 2 advertises, and an
  // official tool of the same name always wins.
  assert.match(proxy, /fn merge_local_tools\(/);
  assert.match(proxy, /if existing\.contains\(name\) \{\s*continue;/);
  assert.match(proxy, /fn call_local_tool<'a>\(/);
  for (const tool of [
    'add_timing_marker',
    'add_timing_marker_pair',
    'list_timing_markers',
    'set_timing_marker_note',
    'remove_timing_marker',
  ]) {
    assert.ok(backend.includes(`"${tool}"`), `${tool} must be defined and classified`);
  }

  // Shadowing has to hold on both halves. The listing suppresses a local tool whose name
  // Logic 2 serves; dispatch has to yield on that same name, or the agent reads one
  // schema and reaches another implementation.
  assert.match(proxy, /fn upstream_tool_names\(/);
  assert.match(proxy, /fn observe_upstream_tools\(/);
  assert.match(backend, /mcp_upstream_tools/);
  assert.match(backend, /if shadowed_by_upstream \{\s*return None;/);

  // Only colours Logic 2 renders may be advertised: it looks the name up in its own map
  // and silently drops an unknown one, so a dead enum entry is a promise that is not kept.
  for (const dead of ['"blue"', '"pink"', '"teal"']) {
    assert.ok(
      !backend.includes(dead),
      `${dead} is not a colour Logic 2 renders and must not be offered`,
    );
  }
  assert.ok(backend.includes('"paleRed"'), 'the palette Logic 2 uses itself must be offered');

  // Annotating a capture cannot lose sample data, so these are not gated -- but they
  // must be named, or the unknown branch would ask about every note.
  const policyStart = backend.indexOf('fn mcp_tool_policy');
  const policyEnd = backend.indexOf('\n}', policyStart);
  const policy = backend.slice(policyStart, policyEnd);
  assert.match(policy, /"add_timing_marker"/);
  assert.match(policy, /"remove_timing_marker" => McpToolPolicy::Allow/);

  // The marker request rides the channel that already exists, and every request is
  // answered: a caller must never be left waiting on a session that has gone.
  assert.match(backend, /fn call_renderer\(/);
  assert.match(backend, /fn abandon_renderer_requests\(/);
  assert.match(backend, /const RENDERER_REQUEST_TIMEOUT: Duration/);
  assert.match(index, /isMarkerCommand\(parsed\.type\)/);

  // A store path Logic 2 never published: a version that moves it has to say so rather
  // than look like an empty capture.
  assert.match(markers, /activeSessionOptional \?\? store\.activeSession/);
  assert.match(markers, /may have moved it/);
  assert.match(markers, /no active capture session/);
  // Notes are data. They are escaped into the expression, never concatenated raw.
  assert.match(markers, /function quote\(value\)/);
  assert.match(markers, /JSON\.stringify\(String\(value\)\)/);
  // Both line terminators JSON leaves bare are escaped, and as escapes rather than as
  // literal characters -- a literal one inside a regex ends the regex.
  assert.match(markers, /\.replace\(\/\\u2028\/g, '\\\\u2028'\)/);
  assert.match(markers, /\.replace\(\/\\u2029\/g, '\\\\u2029'\)/);

  // Logic 2's own gate on annotations is respected rather than written through: for a
  // non-MSO device it is `captureFinished`, so the app itself never annotates a running
  // capture. Only an explicit false refuses, so a build without the property still works.
  assert.match(markers, /canAddAnnotations === false/);
  assert.match(markers, /const ANNOTATABLE_SESSION/);

  // Pairs share the marker sidebar and the id sequence, so listing reads both maps and an
  // id resolves to either. Reading only `markers` reported a capture holding a pair as
  // empty.
  assert.match(markers, /store\.pairs \?\? \{\}/);
  assert.match(markers, /createPairFromMarkers/);
  assert.match(markers, /no timing marker or pair with id/);
  assert.match(markers, /durationSec: item\.timesSec\[1\] - item\.timesSec\[0\]/);
});
