// Arch R Flasher — Frontend Logic
// Tauri 2 IPC: all backend calls go through invoke()

// ---------------------------------------------------------------------------
// i18n
// ---------------------------------------------------------------------------
let lang = {};
const SUPPORTED_LOCALES = ['en', 'pt-BR', 'es', 'zh'];

async function initI18n() {
  try {
    const osLocale = await window.__TAURI__.core.invoke('get_locale');
    const normalized = osLocale.replace('_', '-');
    let locale = SUPPORTED_LOCALES.find(l => normalized.startsWith(l));
    if (!locale) {
      const langPart = normalized.split('-')[0];
      locale = SUPPORTED_LOCALES.find(l => l.startsWith(langPart)) || 'en';
    }

    const resp = await fetch(`i18n/${locale}.json`);
    lang = await resp.json();
  } catch (e) {
    try {
      const resp = await fetch('i18n/en.json');
      lang = await resp.json();
    } catch (_) {
      lang = {};
    }
  }

  applyI18n();
}

function t(key, replacements) {
  let text = lang[key] || key;
  if (replacements) {
    for (const [k, v] of Object.entries(replacements)) {
      text = text.replace(`{${k}}`, v);
    }
  }
  return text;
}

function applyI18n() {
  document.querySelectorAll('[data-i18n]').forEach(el => {
    const key = el.getAttribute('data-i18n');
    if (lang[key]) el.textContent = lang[key];
  });
  document.querySelectorAll('[data-i18n-title]').forEach(el => {
    const key = el.getAttribute('data-i18n-title');
    if (lang[key]) el.title = lang[key];
  });
}

// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------
document.querySelectorAll('.tab').forEach(tab => {
  tab.addEventListener('click', () => {
    if (busy) return;
    document.querySelectorAll('.tab').forEach(t => t.classList.remove('active'));
    document.querySelectorAll('.tab-content').forEach(c => c.classList.remove('active'));
    tab.classList.add('active');
    const contentId = 'content-' + tab.id.replace('tab-', '');
    document.getElementById(contentId).classList.add('active');
  });
});

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
let selectedConsole = null;
let selectedPanel = null;
let selectedDisk = null;
let imagePath = null;
let busy = false;

// ---------------------------------------------------------------------------
// DOM — Flash tab
// ---------------------------------------------------------------------------
const $ = (id) => document.getElementById(id);
const btnOriginal = $('btn-original');
const btnClone = $('btn-clone');
const panelSection = $('panel-section');
const panelSelect = $('panel-select');
const diskSection = $('disk-section');
const diskSelect = $('disk-select');
const flashSection = $('flash-section');
const btnFlash = $('btn-flash');
const progressSection = $('progress-section');
const progressFill = $('progress-fill');
const progressPercent = $('progress-percent');
const progressStage = $('progress-stage');
const statusEl = $('status');
const imageNameEl = $('image-name');
const imageVersionEl = $('image-version');
const confirmDialog = $('confirm-dialog');
const confirmText = $('confirm-text');

// ---------------------------------------------------------------------------
// Busy state — disables all controls during operations
// ---------------------------------------------------------------------------
function setBusy(isBusy) {
  busy = isBusy;
  const controls = [
    btnOriginal, btnClone, panelSelect, diskSelect,
    $('btn-select-file'), $('btn-download'), $('btn-refresh-disks'),
  ];
  controls.forEach(el => { if (el) el.disabled = isBusy; });
  document.querySelectorAll('.tab').forEach(t => t.disabled = isBusy);
  updateFlashButton();
}

// ---------------------------------------------------------------------------
// Console selection
// ---------------------------------------------------------------------------
function selectConsole(console) {
  if (busy) return;
  const changed = selectedConsole !== console;
  selectedConsole = console;
  selectedPanel = null;

  btnOriginal.classList.toggle('active', console === 'original');
  btnClone.classList.toggle('active', console === 'clone');

  $('image-section').style.display = '';

  if (changed && imagePath) {
    imagePath = null;
    imageNameEl.textContent = t('no_image');
    imageNameEl.setAttribute('data-i18n', 'no_image');
    imageNameEl.style.color = '';
    imageVersionEl.textContent = '';
  }

  loadPanels(console);
  panelSection.style.display = '';
  $('customization-section').style.display = 'none';
  diskSection.style.display = 'none';
  flashSection.style.display = 'none';
  updateFlashButton();
}

btnOriginal.addEventListener('click', () => selectConsole('original'));
btnClone.addEventListener('click', () => selectConsole('clone'));

// ---------------------------------------------------------------------------
// Panel loading
// ---------------------------------------------------------------------------
async function loadPanels(console, selectEl) {
  const target = selectEl || panelSelect;
  const panels = await window.__TAURI__.core.invoke('get_panels', { console });

  target.innerHTML = `<option value="">${t('select_panel')}</option>`;

  panels.forEach(panel => {
    const opt = document.createElement('option');
    opt.value = JSON.stringify({ id: panel.id, dtbo: panel.dtbo });
    opt.textContent = panel.name;
    target.appendChild(opt);
  });

  if (target === panelSelect) selectedPanel = null;
}

panelSelect.addEventListener('change', () => {
  if (panelSelect.value) {
    selectedPanel = JSON.parse(panelSelect.value);
    onPanelSelected();
  } else {
    selectedPanel = null;
    diskSection.style.display = 'none';
    flashSection.style.display = 'none';
  }
  updateFlashButton();
});

function onPanelSelected() {
  $('customization-section').style.display = '';
  diskSection.style.display = '';
  flashSection.style.display = '';
  refreshDisks();
}

// ---------------------------------------------------------------------------
// Disk listing
// ---------------------------------------------------------------------------
async function refreshDisks() {
  const disks = await window.__TAURI__.core.invoke('list_disks');

  diskSelect.innerHTML = `<option value="">${t('select_sd')}</option>`;
  selectedDisk = null;

  if (disks.length === 0) {
    const opt = document.createElement('option');
    opt.value = '';
    opt.textContent = t('no_sd');
    opt.disabled = true;
    diskSelect.appendChild(opt);
  } else {
    disks.forEach(disk => {
      const opt = document.createElement('option');
      opt.value = disk.device;
      opt.textContent = disk.name;
      diskSelect.appendChild(opt);
    });
  }

  updateFlashButton();
}

diskSelect.addEventListener('change', () => {
  selectedDisk = diskSelect.value || null;
  updateFlashButton();
});

$('btn-refresh-disks').addEventListener('click', refreshDisks);

// ---------------------------------------------------------------------------
// Flash button state
// ---------------------------------------------------------------------------
function updateFlashButton() {
  btnFlash.disabled = busy || !(imagePath && selectedConsole && selectedPanel && selectedDisk);
}

// ---------------------------------------------------------------------------
// File selection (local file picker)
// ---------------------------------------------------------------------------
$('btn-select-file').addEventListener('click', async () => {
  if (busy) return;
  try {
    const selected = await window.__TAURI__.dialog.open({
      filters: [{
        name: 'Arch R Image',
        extensions: ['img', 'xz', 'gz']
      }]
    });

    if (selected) {
      imagePath = selected;
      const fileName = selected.split(/[/\\]/).pop();
      imageNameEl.textContent = fileName;
      imageNameEl.removeAttribute('data-i18n');
      imageNameEl.style.color = 'var(--text)';
      imageVersionEl.textContent = '';
      updateFlashButton();
    }
  } catch (e) {
    setStatus(t('error_select_file') + e, 'error');
  }
});

// ---------------------------------------------------------------------------
// Download latest (in-app download with progress)
// ---------------------------------------------------------------------------
$('btn-download').addEventListener('click', async () => {
  if (busy) return;
  setBusy(true);
  progressSection.style.display = '';
  progressFill.style.width = '0%';
  progressPercent.textContent = '0%';
  progressStage.textContent = t('checking_version');
  setStatus(t('checking_version'), '');

  try {
    const result = await window.__TAURI__.core.invoke('download_image', { variant: selectedConsole });

    imagePath = result.path;
    imageNameEl.textContent = result.image_name;
    imageNameEl.removeAttribute('data-i18n');
    imageNameEl.style.color = 'var(--text)';
    imageVersionEl.textContent = result.version;

    if (result.cached) {
      setStatus(t('cached'), 'success');
    } else {
      setStatus(t('download_complete'), 'success');
    }

    progressFill.style.width = '100%';
    progressPercent.textContent = '100%';
    progressStage.textContent = '';

    setTimeout(() => {
      if (!busy) progressSection.style.display = 'none';
    }, 2000);
  } catch (e) {
    setStatus(translateError(e), 'error');
    progressSection.style.display = 'none';
  }

  setBusy(false);
  updateFlashButton();
});

// Download progress listener
window.__TAURI__.event.listen('download-progress', (event) => {
  const { percent, downloaded_bytes, total_bytes } = event.payload;
  progressFill.style.width = percent.toFixed(1) + '%';
  progressPercent.textContent = percent.toFixed(0) + '%';

  const dl = formatBytes(downloaded_bytes);
  const tot = formatBytes(total_bytes);
  progressStage.textContent = `${t('downloading')} ${dl} / ${tot}`;
});

// ---------------------------------------------------------------------------
// Flash
// ---------------------------------------------------------------------------
$('btn-flash').addEventListener('click', () => {
  if (busy) return;
  const diskName = diskSelect.options[diskSelect.selectedIndex].textContent;
  confirmText.textContent = t('confirm_text', { disk: diskName });
  confirmDialog.style.display = '';
});

$('btn-cancel').addEventListener('click', () => {
  confirmDialog.style.display = 'none';
});

$('btn-confirm').addEventListener('click', async () => {
  confirmDialog.style.display = 'none';
  await startFlash();
});

async function startFlash() {
  setBusy(true);
  progressSection.style.display = '';
  progressFill.style.width = '0%';
  progressPercent.textContent = '0%';
  progressStage.textContent = t('writing');
  setStatus(t('writing'), '');

  try {
    await window.__TAURI__.core.invoke('flash_image', {
      imagePath: imagePath,
      device: selectedDisk,
      panelDtbo: selectedPanel.dtbo,
      variant: selectedConsole,
      rotation: parseInt($('rotation-select').value) || 0,
      invertLeftStick: $('invert-lstick').checked,
      invertRightStick: $('invert-rstick').checked,
      hpInvert: $('hp-invert').checked,
    });

    progressFill.style.width = '100%';
    progressPercent.textContent = '100%';
    progressStage.textContent = '';
    setStatus(t('done'), 'success');
  } catch (e) {
    const msg = typeof e === 'string' ? e : String(e);
    if (msg === 'cancelled') {
      setStatus(t('flash_cancelled'), '');
    } else {
      setStatus(translateError(msg), 'error');
    }
    progressSection.style.display = 'none';
  }

  setBusy(false);
}

// Flash progress listener
window.__TAURI__.event.listen('flash-progress', (event) => {
  const { percent, stage } = event.payload;
  progressFill.style.width = percent.toFixed(1) + '%';
  progressPercent.textContent = percent.toFixed(0) + '%';
  progressStage.textContent = t(stage) || stage;
});

// ---------------------------------------------------------------------------
// OVERLAY TAB
// ---------------------------------------------------------------------------
const overlaySdSelect = $('overlay-sd-select');
const overlayPanelSelect = $('overlay-panel-select');
const btnApplyOverlay = $('btn-apply-overlay');
const overlayStatusEl = $('overlay-status');

let overlayBootPath = null;
let overlaySelectedPanel = null;
let overlayConsole = null;

// Scan for Arch R SD cards
async function refreshOverlaySD() {
  try {
    const partitions = await window.__TAURI__.core.invoke('find_archr_sd');

    overlaySdSelect.innerHTML = '';

    if (partitions.length === 0) {
      const opt = document.createElement('option');
      opt.value = '';
      opt.textContent = t('overlay_no_sd');
      opt.disabled = true;
      overlaySdSelect.appendChild(opt);
      overlayBootPath = null;
      $('overlay-info-section').style.display = 'none';
      $('overlay-panel-section').style.display = 'none';
      $('overlay-customization-section').style.display = 'none';
      $('overlay-apply-section').style.display = 'none';
    } else {
      const placeholder = document.createElement('option');
      placeholder.value = '';
      placeholder.textContent = t('select_sd');
      overlaySdSelect.appendChild(placeholder);

      partitions.forEach(p => {
        const opt = document.createElement('option');
        opt.value = p;
        opt.textContent = p;
        overlaySdSelect.appendChild(opt);
      });

      // Auto-select if only one
      if (partitions.length === 1) {
        overlaySdSelect.value = partitions[0];
        await onOverlaySDSelected(partitions[0]);
      }
    }
  } catch (e) {
    setOverlayStatus(t('error') + ': ' + e, 'error');
  }
}

overlaySdSelect.addEventListener('change', async () => {
  const val = overlaySdSelect.value;
  if (val) {
    await onOverlaySDSelected(val);
  } else {
    overlayBootPath = null;
    $('overlay-info-section').style.display = 'none';
    $('overlay-panel-section').style.display = 'none';
    $('overlay-customization-section').style.display = 'none';
    $('overlay-apply-section').style.display = 'none';
  }
});

async function onOverlaySDSelected(bootPath) {
  overlayBootPath = bootPath;

  try {
    const status = await window.__TAURI__.core.invoke('read_overlay', { bootPath });

    if (!status.has_archr) {
      setOverlayStatus(t('overlay_not_archr'), 'error');
      return;
    }

    // Show current overlay info
    $('overlay-current-name').textContent = status.current_panel_name || t('overlay_none');
    $('overlay-current-file').textContent = status.current_overlay || t('overlay_none');
    $('overlay-current-variant').textContent = status.variant || '—';
    $('overlay-current-rotation').textContent = status.rotation + '°';
    $('overlay-current-lstick').textContent = status.invert_left_stick ? 'Yes' : 'No';
    $('overlay-current-rstick').textContent = status.invert_right_stick ? 'Yes' : 'No';
    $('overlay-current-hp').textContent = status.hp_invert ? 'Yes' : 'No';
    $('overlay-info-section').style.display = '';
    $('overlay-panel-section').style.display = '';
    $('overlay-customization-section').style.display = '';
    $('overlay-apply-section').style.display = '';

    // Pre-fill customization controls with current config
    $('overlay-rotation-select').value = String(status.rotation || 0);
    $('overlay-invert-lstick').checked = status.invert_left_stick || false;
    $('overlay-invert-rstick').checked = status.invert_right_stick || false;
    $('overlay-hp-invert').checked = status.hp_invert || false;

    // Auto-select console based on variant
    if (status.variant === 'original' || status.variant === 'clone') {
      selectOverlayConsole(status.variant);
    }

    setOverlayStatus('', '');
  } catch (e) {
    setOverlayStatus(t('error') + ': ' + e, 'error');
  }
}

$('btn-refresh-overlay-sd').addEventListener('click', refreshOverlaySD);

// Overlay console selection
function selectOverlayConsole(console) {
  overlayConsole = console;
  $('btn-overlay-original').classList.toggle('active', console === 'original');
  $('btn-overlay-clone').classList.toggle('active', console === 'clone');
  overlayPanelSelect.style.display = '';
  overlaySelectedPanel = null;
  btnApplyOverlay.disabled = true;
  loadPanels(console, overlayPanelSelect);
}

$('btn-overlay-original').addEventListener('click', () => selectOverlayConsole('original'));
$('btn-overlay-clone').addEventListener('click', () => selectOverlayConsole('clone'));

overlayPanelSelect.addEventListener('change', () => {
  if (overlayPanelSelect.value) {
    overlaySelectedPanel = JSON.parse(overlayPanelSelect.value);
    btnApplyOverlay.disabled = !overlayBootPath;
  } else {
    overlaySelectedPanel = null;
    btnApplyOverlay.disabled = true;
  }
});

// Apply overlay
btnApplyOverlay.addEventListener('click', async () => {
  if (!overlayBootPath || !overlaySelectedPanel || !overlayConsole) return;

  btnApplyOverlay.disabled = true;
  setOverlayStatus(t('overlay_applying'), '');

  try {
    await window.__TAURI__.core.invoke('apply_panel_with_config', {
      bootPath: overlayBootPath,
      panelDtbo: overlaySelectedPanel.dtbo,
      variant: overlayConsole,
      rotation: parseInt($('overlay-rotation-select').value) || 0,
      invertLeftStick: $('overlay-invert-lstick').checked,
      invertRightStick: $('overlay-invert-rstick').checked,
      hpInvert: $('overlay-hp-invert').checked,
    });

    // Refresh current overlay info, then show success
    await onOverlaySDSelected(overlayBootPath);
    setOverlayStatus(t('overlay_applied'), 'success');
  } catch (e) {
    setOverlayStatus(t('error') + ': ' + e, 'error');
    btnApplyOverlay.disabled = false;
  }
});

function setOverlayStatus(text, type) {
  overlayStatusEl.textContent = text;
  overlayStatusEl.className = 'status' + (type ? ' ' + type : '');
}

// Auto-refresh overlay SD when switching to overlay tab
$('tab-overlay').addEventListener('click', () => {
  refreshOverlaySD();
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
function setStatus(text, type) {
  statusEl.textContent = text;
  statusEl.className = 'status' + (type ? ' ' + type : '');
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
    if (regex.test(msg)) {
      if (key === null) break;
      return t(key);
    }
  }
  return t('error') + ': ' + msg;
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------
async function init() {
  await initI18n();
  checkForAppUpdate();
}

async function checkForAppUpdate() {
  try {
    const result = await window.__TAURI__.core.invoke('check_app_update');
    if (!result) return;

    const [version, ...rest] = result.split('|');
    const yes = await window.__TAURI__.dialog.ask(
      t('app_update_text', { version }),
      { title: t('app_update_title'), kind: 'info' }
    );
    if (!yes) return;

    setStatus(t('app_updating'), '');
    await window.__TAURI__.core.invoke('install_app_update');
  } catch (_) {
    // offline or error — ignore silently
  }
}

init();
