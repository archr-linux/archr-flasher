// Arch R Flasher — Wizard Frontend
// Tauri 2 IPC: all backend calls go through invoke()

const $ = (id) => document.getElementById(id);
const invoke = window.__TAURI__?.core?.invoke || (async () => {});

// ---------------------------------------------------------------------------
// i18n
// ---------------------------------------------------------------------------
let lang = {};
const SUPPORTED_LOCALES = ['en', 'pt-BR', 'es', 'zh', 'ru'];

async function initI18n() {
  try {
    const osLocale = await invoke('get_locale');
    const normalized = (osLocale || 'en').replace('_', '-');
    let locale = SUPPORTED_LOCALES.find(l => normalized.startsWith(l));
    if (!locale) {
      const langPart = normalized.split('-')[0];
      locale = SUPPORTED_LOCALES.find(l => l.startsWith(langPart)) || 'en';
    }
    const resp = await fetch(`i18n/${locale}.json`);
    lang = await resp.json();
  } catch (_) {
    try { const r = await fetch('i18n/en.json'); lang = await r.json(); } catch (_) {}
  }
  applyI18n();
}

function t(key, rep) {
  let s = lang[key] || key;
  if (rep) for (const [k, v] of Object.entries(rep)) s = s.replace(`{${k}}`, v);
  return s;
}

function applyI18n() {
  document.querySelectorAll('[data-i18n]').forEach(el => {
    const k = el.getAttribute('data-i18n');
    if (lang[k]) el.textContent = lang[k];
  });
  document.querySelectorAll('[data-i18n-title]').forEach(el => {
    const k = el.getAttribute('data-i18n-title');
    if (lang[k]) el.title = lang[k];
  });
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
let mode = 'flash';  // 'flash' or 'overlay'
let currentStep = 0;
let busy = false;

// Flash state
let selectedConsole = null;
let selectedPanel = null;
let selectedDisk = null;
let imagePath = null;

// Overlay state
let overlayBootPath = null;
let overlayConsole = null;
let overlaySelectedPanel = null;

const FLASH_STEPS = ['console', 'image', 'panel', 'customize', 'disk', 'flash'];
const OVERLAY_STEPS = ['ovl-detect', 'ovl-panel', 'ovl-customize', 'ovl-apply'];

const STEP_TITLES = {
  'console': 'select_console', 'image': 'select_image', 'panel': 'select_panel_title',
  'customize': 'step_customize', 'disk': 'select_sd_title', 'flash': 'step_flash',
  'ovl-detect': 'overlay_sd', 'ovl-panel': 'overlay_new_panel',
  'ovl-customize': 'step_customize', 'ovl-apply': 'step_apply',
};

function getSteps() { return mode === 'flash' ? FLASH_STEPS : OVERLAY_STEPS; }
function getStepId() { return getSteps()[currentStep]; }

// ---------------------------------------------------------------------------
// Wizard navigation
// ---------------------------------------------------------------------------
function goToStep(idx) {
  const steps = getSteps();
  if (idx < 0 || idx >= steps.length) return;
  currentStep = idx;
  updateUI();
}

function nextStep() {
  const steps = getSteps();
  if (currentStep < steps.length - 1) {
    // Execute step-specific actions before advancing
    const stepId = getStepId();
    if (mode === 'flash') {
      if (stepId === 'panel') onPanelSelected();
      if (stepId === 'disk') onDiskReady();
    }
    if (mode === 'overlay') {
      if (stepId === 'ovl-customize') buildOverlaySummary();
    }
    goToStep(currentStep + 1);
  }
}

function prevStep() {
  if (currentStep > 0) goToStep(currentStep - 1);
}

function updateUI() {
  const steps = getSteps();
  const stepId = getStepId();
  const stepsNav = mode === 'flash' ? 'steps-flash' : 'steps-overlay';

  // Update sidebar steps
  $('steps-flash').classList.toggle('hidden', mode !== 'flash');
  $('steps-overlay').classList.toggle('hidden', mode !== 'overlay');

  document.querySelectorAll(`#${stepsNav} .step`).forEach((el, i) => {
    el.classList.toggle('active', i === currentStep);
    el.classList.toggle('done', i < currentStep);
  });

  // Update main content
  document.querySelectorAll('.step-content').forEach(el => el.classList.remove('active'));
  const page = $('page-' + stepId);
  if (page) page.classList.add('active');

  // Update title
  const titleKey = STEP_TITLES[stepId] || stepId;
  $('step-title').textContent = t(titleKey);

  // Update nav buttons
  $('btn-back').classList.toggle('hidden', currentStep === 0);
  updateNextButton();

  // Mode tabs
  $('mode-flash').classList.toggle('active', mode === 'flash');
  $('mode-overlay').classList.toggle('active', mode === 'overlay');
}

function updateNextButton() {
  const stepId = getStepId();
  const btn = $('btn-next');

  if (mode === 'flash') {
    switch (stepId) {
      case 'console': btn.disabled = !selectedConsole; break;
      case 'image': btn.disabled = !imagePath; break;
      case 'panel': btn.disabled = !selectedPanel; break;
      case 'customize': btn.disabled = false; break;
      case 'disk': btn.disabled = !selectedDisk; break;
      case 'flash':
        btn.textContent = t('flash');
        btn.disabled = busy;
        break;
      default: btn.disabled = false;
    }
  } else {
    switch (stepId) {
      case 'ovl-detect': btn.disabled = !overlayBootPath; break;
      case 'ovl-panel': btn.disabled = !overlaySelectedPanel; break;
      case 'ovl-customize': btn.disabled = false; break;
      case 'ovl-apply':
        btn.textContent = t('overlay_apply');
        btn.disabled = busy;
        break;
      default: btn.disabled = false;
    }
  }

  // Last step: change button text
  const steps = getSteps();
  if (currentStep === steps.length - 1) {
    if (mode === 'flash') btn.textContent = t('flash') || 'FLASH';
    else btn.textContent = t('overlay_apply') || 'APPLY';
  } else {
    btn.textContent = t('next') || 'NEXT';
  }
}

// ---------------------------------------------------------------------------
// Mode switching
// ---------------------------------------------------------------------------
$('mode-flash').addEventListener('click', () => {
  if (busy) return;
  mode = 'flash';
  currentStep = 0;
  updateUI();
});

$('mode-overlay').addEventListener('click', () => {
  if (busy) return;
  mode = 'overlay';
  currentStep = 0;
  updateUI();
  refreshOverlaySD();
});

// ---------------------------------------------------------------------------
// Nav buttons
// ---------------------------------------------------------------------------
$('btn-back').addEventListener('click', () => { if (!busy) prevStep(); });

$('btn-next').addEventListener('click', () => {
  if (busy) return;
  const steps = getSteps();
  if (currentStep === steps.length - 1) {
    // Last step action
    if (mode === 'flash') showFlashConfirm();
    else applyOverlay();
  } else {
    nextStep();
  }
});

// ---------------------------------------------------------------------------
// Console selection (Flash step 1)
// ---------------------------------------------------------------------------
document.querySelectorAll('#page-console .card').forEach(card => {
  card.addEventListener('click', () => {
    if (busy) return;
    const console = card.dataset.console;
    const changed = selectedConsole !== console;
    selectedConsole = console;

    document.querySelectorAll('#page-console .card').forEach(c => c.classList.remove('active'));
    card.classList.add('active');

    if (changed) {
      imagePath = null;
      $('image-name').textContent = t('no_image');
      $('image-version').textContent = '';
      selectedPanel = null;
      checkLatestVersion(console);
    }

    loadPanels(console, $('panel-select'));
    updateNextButton();
  });
});

// ---------------------------------------------------------------------------
// Panel loading
// ---------------------------------------------------------------------------
async function loadPanels(console, selectEl) {
  try {
    const panels = await invoke('get_panels', { console });
    selectEl.innerHTML = `<option value="">${t('select_panel')}</option>`;
    panels.forEach(p => {
      const opt = document.createElement('option');
      opt.value = JSON.stringify({ id: p.id, dtbo: p.dtbo });
      opt.textContent = p.name;
      selectEl.appendChild(opt);
    });
  } catch (_) {}
}

// Flash panel select
$('panel-select').addEventListener('change', () => {
  const val = $('panel-select').value;
  selectedPanel = val ? JSON.parse(val) : null;
  updateNextButton();
});

// ---------------------------------------------------------------------------
// Latest version check
// ---------------------------------------------------------------------------
async function checkLatestVersion(console) {
  $('image-version').textContent = t('checking_version');
  try {
    const variant = console === 'soysauce' ? 'original' : console;
    const release = await invoke('check_latest_release', { variant });
    if (selectedConsole === console) {
      $('image-version').textContent = t('latest_version', { version: release.version });
    }
  } catch (_) {
    if (selectedConsole === console) $('image-version').textContent = t('offline');
  }
}

// ---------------------------------------------------------------------------
// Image selection
// ---------------------------------------------------------------------------
$('btn-select-file').addEventListener('click', async () => {
  if (busy) return;
  try {
    const selected = await window.__TAURI__.dialog.open({
      filters: [{ name: 'Arch R Image', extensions: ['img', 'xz', 'gz'] }]
    });
    if (selected) {
      imagePath = selected;
      $('image-name').textContent = selected.split(/[/\\]/).pop();
      $('image-name').style.color = 'var(--text)';
      $('image-version').textContent = '';
      updateNextButton();
    }
  } catch (e) { setFlashStatus(t('error') + ': ' + e, 'error'); }
});

$('btn-download').addEventListener('click', async () => {
  if (busy) return;
  setBusy(true);
  const dlProg = $('download-progress');
  dlProg.classList.remove('hidden');
  $('dl-progress-fill').style.width = '0%';
  $('dl-progress-text').textContent = '0%';

  try {
    // Soysauce uses original image
    const variant = selectedConsole === 'soysauce' ? 'original' : selectedConsole;
    const result = await invoke('download_image', { variant });
    imagePath = result.path;
    $('image-name').textContent = result.image_name;
    $('image-name').style.color = 'var(--text)';
    $('image-version').textContent = result.version;
    $('dl-progress-fill').style.width = '100%';
    $('dl-progress-text').textContent = '100%';
    setTimeout(() => dlProg.classList.add('hidden'), 2000);
  } catch (e) {
    setFlashStatus(translateError(e), 'error');
    dlProg.classList.add('hidden');
  }
  setBusy(false);
  updateNextButton();
});

window.__TAURI__?.event?.listen('download-progress', (event) => {
  const { percent, downloaded_bytes, total_bytes } = event.payload;
  $('dl-progress-fill').style.width = percent.toFixed(1) + '%';
  $('dl-progress-text').textContent = percent.toFixed(0) + '%';
});

// ---------------------------------------------------------------------------
// Disk selection (Flash step 5)
// ---------------------------------------------------------------------------
async function refreshDisks() {
  try {
    const disks = await invoke('list_disks');
    const sel = $('disk-select');
    sel.innerHTML = `<option value="">${t('select_sd')}</option>`;
    selectedDisk = null;
    disks.forEach(d => {
      const opt = document.createElement('option');
      opt.value = d.device;
      opt.textContent = d.name;
      sel.appendChild(opt);
    });
  } catch (_) {}
  updateNextButton();
}

$('disk-select').addEventListener('change', () => {
  selectedDisk = $('disk-select').value || null;
  updateNextButton();
});

$('btn-refresh-disks').addEventListener('click', refreshDisks);

function onPanelSelected() { refreshDisks(); }
function onDiskReady() { buildFlashSummary(); }

// ---------------------------------------------------------------------------
// Flash summary & execution
// ---------------------------------------------------------------------------
const CONSOLE_I18N = { original: 'original_name', clone: 'clone_name', soysauce: 'soysauce_name' };

function buildFlashSummary() {
  const consoleName = t(CONSOLE_I18N[selectedConsole] || selectedConsole);
  const panelName = selectedPanel ? $('panel-select').options[$('panel-select').selectedIndex].textContent : t('overlay_none');
  const diskName = selectedDisk ? $('disk-select').options[$('disk-select').selectedIndex].textContent : t('overlay_none');
  const imgName = $('image-name').textContent;
  $('flash-summary').innerHTML = `
    <strong>${t('step_console')}:</strong> ${consoleName}<br>
    <strong>${t('step_image')}:</strong> ${imgName}<br>
    <strong>${t('step_panel')}:</strong> ${panelName}<br>
    <strong>${t('rotation')}:</strong> ${$('rotation-select').value}°<br>
    <strong>${t('step_disk')}:</strong> ${diskName}
  `;
}

function showFlashConfirm() {
  buildFlashSummary();
  const diskName = $('disk-select').options[$('disk-select').selectedIndex]?.textContent || selectedDisk;
  $('confirm-text').textContent = t('confirm_text', { disk: diskName });
  $('confirm-dialog').classList.remove('hidden');
}

$('btn-cancel').addEventListener('click', () => $('confirm-dialog').classList.add('hidden'));

$('btn-confirm').addEventListener('click', async () => {
  $('confirm-dialog').classList.add('hidden');
  await startFlash();
});

async function startFlash() {
  setBusy(true);
  $('flash-progress').classList.remove('hidden');
  $('progress-fill').style.width = '0%';
  $('progress-percent').textContent = '0%';
  $('progress-stage').textContent = t('writing');
  setFlashStatus(t('writing'), '');

  try {
    const variant = selectedConsole === 'soysauce' ? 'original' : selectedConsole;
    const panelDtbo = selectedPanel.dtbo === '__custom__' ? selectedPanel._customDtboPath : selectedPanel.dtbo;

    await invoke('flash_image', {
      imagePath, device: selectedDisk,
      panelDtbo, variant,
      rotation: parseInt($('rotation-select').value) || 0,
      invertLeftStick: $('invert-lstick').checked,
      invertRightStick: $('invert-rstick').checked,
      hpInvert: $('hp-invert').checked,
    });
    $('progress-fill').style.width = '100%';
    $('progress-percent').textContent = '100%';
    $('progress-stage').textContent = '';
    setFlashStatus(t('done'), 'success');
  } catch (e) {
    const msg = typeof e === 'string' ? e : String(e);
    if (msg === 'cancelled') setFlashStatus(t('flash_cancelled'), '');
    else setFlashStatus(translateError(msg), 'error');
  }
  setBusy(false);
}

window.__TAURI__?.event?.listen('flash-progress', (event) => {
  const { percent, stage } = event.payload;
  $('progress-fill').style.width = percent.toFixed(1) + '%';
  $('progress-percent').textContent = percent.toFixed(0) + '%';
  $('progress-stage').textContent = t(stage) || stage;
});

// ---------------------------------------------------------------------------
// OVERLAY MODE
// ---------------------------------------------------------------------------

// Overlay: Detect SD (step 1)
async function refreshOverlaySD() {
  try {
    const parts = await invoke('find_archr_sd');
    const sel = $('overlay-sd-select');
    sel.innerHTML = '';
    overlayBootPath = null;

    if (parts.length === 0) {
      const opt = document.createElement('option');
      opt.value = '';
      opt.textContent = t('overlay_no_sd');
      opt.disabled = true;
      sel.appendChild(opt);
    } else {
      sel.innerHTML = `<option value="">${t('select_sd')}</option>`;
      parts.forEach(p => {
        const opt = document.createElement('option');
        opt.value = p; opt.textContent = p;
        sel.appendChild(opt);
      });
      if (parts.length === 1) {
        sel.value = parts[0];
        await onOverlaySDSelected(parts[0]);
      }
    }
  } catch (e) { setOverlayStatus(t('error') + ': ' + e, 'error'); }
  updateNextButton();
}

$('overlay-sd-select').addEventListener('change', async () => {
  const val = $('overlay-sd-select').value;
  if (val) await onOverlaySDSelected(val);
  else { overlayBootPath = null; $('overlay-current-info').classList.add('hidden'); }
  updateNextButton();
});

async function onOverlaySDSelected(bootPath) {
  overlayBootPath = bootPath;
  try {
    const s = await invoke('read_overlay', { bootPath });
    if (!s.has_archr) { setOverlayStatus(t('overlay_not_archr'), 'error'); return; }

    $('ovl-cur-name').textContent = s.current_panel_name || t('overlay_none');
    $('ovl-cur-variant').textContent = s.variant || t('overlay_none');
    $('ovl-cur-rotation').textContent = (s.rotation || 0) + '°';
    $('ovl-cur-lstick').textContent = s.invert_left_stick ? t('yes') : t('no');
    $('ovl-cur-rstick').textContent = s.invert_right_stick ? t('yes') : t('no');
    $('ovl-cur-hp').textContent = s.hp_invert ? t('yes') : t('no');
    $('overlay-current-info').classList.remove('hidden');

    // Pre-fill customize controls
    $('overlay-rotation-select').value = String(s.rotation || 0);
    $('overlay-invert-lstick').checked = s.invert_left_stick || false;
    $('overlay-invert-rstick').checked = s.invert_right_stick || false;
    $('overlay-hp-invert').checked = s.hp_invert || false;

    if (s.variant) selectOverlayConsole(s.variant);
    setOverlayStatus('', '');
  } catch (e) { setOverlayStatus(t('error') + ': ' + e, 'error'); }
  updateNextButton();
}

$('btn-refresh-overlay-sd').addEventListener('click', refreshOverlaySD);

// Overlay: Panel selection (step 2)
document.querySelectorAll('#page-ovl-panel .card').forEach(card => {
  card.addEventListener('click', () => {
    if (busy) return;
    const con = card.dataset.console;
    selectOverlayConsole(con);
  });
});

function selectOverlayConsole(con) {
  overlayConsole = con;
  document.querySelectorAll('#page-ovl-panel .card').forEach(c =>
    c.classList.toggle('active', c.dataset.console === con)
  );
  overlaySelectedPanel = null;
  loadPanels(con, $('overlay-panel-select'));
  updateNextButton();
}

$('overlay-panel-select').addEventListener('change', () => {
  const val = $('overlay-panel-select').value;
  overlaySelectedPanel = val ? JSON.parse(val) : null;
  updateNextButton();
});

// Overlay: Apply (step 4)
async function applyOverlay() {
  if (!overlayBootPath || !overlaySelectedPanel) return;
  setBusy(true);
  setOverlayStatus(t('overlay_applying'), '');

  try {
    if (overlaySelectedPanel.dtbo === '__custom__' && overlaySelectedPanel._customDtboPath) {
      await invoke('apply_custom_overlay', {
        bootPath: overlayBootPath,
        dtboPath: overlaySelectedPanel._customDtboPath,
      });
    } else {
      await invoke('apply_panel_with_config', {
        bootPath: overlayBootPath,
        panelDtbo: overlaySelectedPanel.dtbo,
        variant: overlayConsole || 'original',
        rotation: parseInt($('overlay-rotation-select').value) || 0,
        invertLeftStick: $('overlay-invert-lstick').checked,
        invertRightStick: $('overlay-invert-rstick').checked,
        hpInvert: $('overlay-hp-invert').checked,
      });
    }

    buildOverlaySummary();
    setOverlayStatus(t('overlay_applied'), 'success');
  } catch (e) { setOverlayStatus(t('error') + ': ' + e, 'error'); }
  setBusy(false);
}

function buildOverlaySummary() {
  const panelName = overlaySelectedPanel ? $('overlay-panel-select').options[$('overlay-panel-select').selectedIndex].textContent : t('overlay_none');
  $('overlay-summary').innerHTML = `
    <strong>${t('step_panel')}:</strong> ${panelName}<br>
    <strong>${t('rotation')}:</strong> ${$('overlay-rotation-select').value}°<br>
    <strong>${t('overlay_variant')}:</strong> ${overlayConsole || t('overlay_none')}
  `;
}

// ---------------------------------------------------------------------------
// Custom DTB → Generate Overlay
// ---------------------------------------------------------------------------
async function handleCustomDTB(statusFn) {
  try {
    const selected = await window.__TAURI__.dialog.open({
      filters: [{ name: 'Device Tree Binary', extensions: ['dtb'] }]
    });
    if (!selected) return null;

    const fileName = selected.split(/[/\\]/).pop();
    statusFn(t('custom_dtb_generating', { file: fileName }), '');

    const dtboPath = await invoke('generate_overlay_from_dtb', {
      dtbPath: selected,
      flags: null,
    });

    statusFn(t('custom_dtb_ready', { file: fileName }), 'success');
    return dtboPath;
  } catch (e) {
    statusFn(t('error') + ': ' + e, 'error');
    return null;
  }
}

$('btn-custom-dtb')?.addEventListener('click', async () => {
  const dtboPath = await handleCustomDTB(setFlashStatus);
  if (dtboPath) {
    // Set as selected panel with the custom DTBO path
    selectedPanel = { id: 'custom', dtbo: '__custom__' };
    selectedPanel._customDtboPath = dtboPath;
    $('panel-select').innerHTML = `<option value="" selected>${t('custom_dtb_overlay')}</option>`;
    updateNextButton();
  }
});

$('btn-ovl-custom-dtb')?.addEventListener('click', async () => {
  const dtboPath = await handleCustomDTB(setOverlayStatus);
  if (dtboPath) {
    overlaySelectedPanel = { id: 'custom', dtbo: '__custom__' };
    overlaySelectedPanel._customDtboPath = dtboPath;
    $('overlay-panel-select').innerHTML = `<option value="" selected>${t('custom_dtb_overlay')}</option>`;
    updateNextButton();
  }
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
function setBusy(b) {
  busy = b;
  updateNextButton();
  document.querySelectorAll('.mode-tab').forEach(t => t.disabled = b);
}

function setFlashStatus(text, type) {
  $('flash-status').textContent = text;
  $('flash-status').className = 'status' + (type ? ' ' + type : '');
}

function setOverlayStatus(text, type) {
  $('overlay-status').textContent = text;
  $('overlay-status').className = 'status' + (type ? ' ' + type : '');
}

function formatBytes(bytes) {
  if (bytes >= 1e9) return (bytes / 1e9).toFixed(1) + ' GB';
  if (bytes >= 1e6) return (bytes / 1e6).toFixed(0) + ' MB';
  return bytes + ' B';
}

function translateError(msg) {
  if (typeof msg !== 'string') msg = String(msg);
  const patterns = [
    [/cancelled|canceled/i, 'flash_cancelled'],
    [/not enough temp space/i, 'error_no_space'],
    [/device not found|was the sd card removed/i, 'error_device_removed'],
    [/not a removable/i, 'error_not_removable'],
    [/no.*image.*found/i, 'error_no_image'],
    [/network|dns|connect|timeout/i, 'error_network'],
    [/checksum.*fail/i, 'error_checksum_failed'],
    [/failed to run pkexec|osascript error|failed to run powershell/i, 'error_privilege'],
    [/not authorized|dismissed/i, 'error_privilege'],
    [/decompress error|xzdecoder/i, 'error_decompress'],
    [/write error|flush error/i, 'error_write_failed'],
    [/cannot open image/i, 'error_open_image'],
    [/cannot write helper|cannot set script|cannot write params|cannot create temp/i, 'error_prepare_flash'],
    [/dd error:/i, null],
    [/flash failed/i, 'error_flash_failed'],
    [/invalid device|invalid disk/i, 'error_invalid_device'],
  ];
  for (const [regex, key] of patterns) {
    if (regex.test(msg)) { if (key === null) break; return t(key); }
  }
  return t('error') + ': ' + msg;
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------
async function init() {
  await initI18n();
  updateUI();

  // Show app version
  try {
    const version = await invoke('get_version');
    if (version) $('app-version').textContent = 'v' + version;
  } catch (_) {}

  try {
    const result = await invoke('check_app_update');
    if (result) {
      const [version] = result.split('|');
      const yes = await window.__TAURI__.dialog.ask(
        t('app_update_text', { version }), { title: t('app_update_title'), kind: 'info' }
      );
      if (yes) { setFlashStatus(t('app_updating'), ''); await invoke('install_app_update'); }
    }
  } catch (_) {}
}

init();
