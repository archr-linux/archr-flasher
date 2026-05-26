#!/usr/bin/env python

# https://pypi.org/project/fdt/
# pip install fdt

import os, sys
import fdt
import math



def prop_default(panel, prop, default):
    try:
        return panel.get_property(prop).value
    except:
        return default

def absfrac(x):
    return abs(x - round(x))

def panel_to_desc(panel, args):
    if 'name' in args:
        g_name = args['name'] + ' '
    else:
        g_name = ''
    comment = args.get('comment')
    acc = []

    delays = [
            prop_default(panel, "prepare-delay-ms", 50),
            prop_default(panel, "reset-delay-ms", 50),
            prop_default(panel, "init-delay-ms", 50),
            prop_default(panel, "enable-delay-ms", 50),
            20  # ready -- no such timeout in legacy dtbs
            ]
    delays_str = ','.join(map(str, delays))

    fmtprop = panel.get_property("dsi,format")
    if fmtprop:
        fmt = ['rgb888', 'rgb666', 'rgb666_packed', 'rgb565'] [fmtprop.value]
    else:
        fmt = 'rgb888'
    lanes = panel.get_property("dsi,lanes").value
    flags = panel.get_property("dsi,flags").value
    flags |= 0x0400


    timings = panel.get_subnode("display-timings")
    if 'Dno' in args['flags']:
        # Skip original mode (it may be broken)
        native = None
    else:
        native = timings.get_property("native-mode").value

    # Collect vendor modes
    modes = {}
    orig_def_fps = None
    for m in timings.nodes:
        clock = round(m.get_property("clock-frequency").value/1000)
        hor = [
                m.get_property("hactive").value,
                m.get_property("hfront-porch").value,
                m.get_property("hsync-len").value,
                m.get_property("hback-porch").value,
                ]
        ver = [
                m.get_property("vactive").value,
                m.get_property("vfront-porch").value,
                m.get_property("vsync-len").value,
                m.get_property("vback-porch").value,
                ]

        mode = {'clock': clock, 'hor': hor, 'ver': ver}
        # Some vendor DTBs (e.g. R36S-V20 2025-05-18) omit `phandle` on
        # individual modes. A mode without a phandle can't be the native
        # default, so skip the comparison instead of crashing.
        phandle_prop = m.get_property("phandle")
        is_native = phandle_prop is not None and phandle_prop.value == native
        if is_native:
            mode['default'] = True

        htotal = sum(hor)
        vtotal = sum(ver)
        fps = clock*1000/(htotal*vtotal)

        if fps not in modes:
            modes[fps] = mode
        if is_native:
            modes[fps]['default'] = True
            orig_def_fps = fps


    try:
        w = panel.get_property("width-mm").value
        h = panel.get_property("height-mm").value
    except:
        if args.diagonal:
            diag_mm = args.diagonal * 25.4
        else:
            diag_mm = 3.5 * 2.54

        randmode = list(modes.values())[0]
        hactive = randmode['hor'][0]
        vactive = randmode['ver'][0]
        pxdiag = math.sqrt(hactive*hactive + vactive*vactive)
        w = round(diag_mm * hactive/pxdiag)
        h = round(diag_mm * vactive/pxdiag)

    # G size=52,70 delays=2,1,20,120,50,20 format=rgb888 lanes=4 flags=0xe03
    acc += [f"G {g_name}size={w},{h} delays={delays_str} format={fmt} lanes={lanes} flags=0x{flags:x}", ""]

    # Based on vendor modes construct a better set of modes
    # https://tasvideos.org/PlatformFramerates
    # 50, 60        -- generic
    # */1.001       -- NTSC hack with 1001 divisor
    # 50.0070       -- PAL NES  https://www.nesdev.org/wiki/Cycle_reference_chart
    # 60.0988       -- NTSC NES
    # 54.8766       -- src/mame/toaplan/twincobr.cpp
    # 57.5          -- src/mame/kaneko/snowbros.cpp
    # 59.7275       -- https://en.wikipedia.org/wiki/Game_Boy
    # 75.47         -- https://ws.nesdev.org/wiki/Display
    def_fps = 60
    if orig_def_fps:
        def_fps = orig_def_fps
    common_fpss = [50/1.001, 50, 50.0070, 57.5, 59.7275, 60/1.001, 60, 60.0988, 75.47, 90, 120];
    common_fpss = [ fps for fps in common_fpss if fps != orig_def_fps]
    for targetfps in [orig_def_fps] + common_fpss:
        if not targetfps:
            continue
        warn = ""
        # nearest fps to base on
        greaterfps = [fps for fps in modes.keys() if fps >= targetfps]
        if greaterfps == []:
            basefps = max(modes.keys())
            basemode = modes[basefps]
            clock = None
        else:
            # Trust original clock. If real clock differs, maybe make a whitelist or blacklist here
            basefps = min(greaterfps)
            basemode = modes[basefps]
            clock = basemode['clock']
        hor = basemode['hor'].copy()
        ver = basemode['ver'].copy()
        # Assume original totals are minimal for the panel at this clock
        htotal = sum(hor)
        vtotal = sum(ver)
        perfectclock = targetfps*htotal*vtotal/1000
        if not clock:
            warn = "(CAN FAIL) "
            # This may fail, but worth trying. Round up to 10kHz
            clock = math.ceil(perfectclock/10)*10
        elif clock > 1.25*perfectclock:
            # Too much deviation may cause no image
            clock = math.ceil(perfectclock/10)*10

        maxvtotal = round(vtotal*1.25)
        # A little bruteforce to find a best totals for target fps
        # TODO: maybe iterate over some clock values too
        options = [(absfrac(c*1000/targetfps/vt), c, vt)
                for vt in range(vtotal, maxvtotal+1)
                for c in range(clock, round(1.25*perfectclock), 10)
                if ((c*1000/targetfps/vt) >= htotal) and ((c*1000/targetfps/vt) < htotal*1.05) ]
        if options == []:
            acc += [f"# failed to find mode for fps={targetfps:.6f} c={clock} h={htotal} v={vtotal}"]
            continue
        (mindev, newclock, newvtotal) = min(options)
        # construct a new mode with chosen vtotal
        newhtotal = round(newclock*1000/targetfps/newvtotal)
        addhtotal = newhtotal - htotal
        addvtotal = newvtotal - vtotal
        expectedfps = newclock*1000/newvtotal/newhtotal
        hor[2] += addhtotal
        ver[2] += addvtotal
        hor_str = ','.join(map(str, hor))
        ver_str = ','.join(map(str, ver))
        maybe_default = " default=1" if targetfps == def_fps else ""
        maybe_comment = f" # {warn}fps={expectedfps:.6f} (target={targetfps:.6f})" if comment else ""
        acc += [f"M clock={newclock} horizontal={hor_str} vertical={ver_str}{maybe_default}{maybe_comment}"]

    acc += [""]

    iseq0 = panel.get_property("panel-init-sequence")
    if (hasattr(iseq0, 'value')) and (isinstance(iseq0.value, (int))):
        iseq = b''.join(map(lambda w : w.to_bytes(4, "big"), list(iseq0)))
    else:
        iseq = bytearray(iseq0)

    while iseq:
        cmd = iseq[0]
        wait = iseq[1]
        datalen = iseq[2]
        iseq = iseq[3:]

        data = iseq[0:datalen]
        iseq = iseq[datalen:]

        maybe_wait = f" wait={wait}" if (wait) else ""
        maybe_comment = f" # orig_cmd=0x{cmd:x}" if comment else ""
        acc += [f"I seq={data.hex()}{maybe_wait}{maybe_comment}"]

    return acc

def node_by_phandle(dt, phandle):
    nodes = [p.parent
             for p in dt.search('phandle', fdt.ItemType.PROP_WORDS)
             if p.value == phandle]
    return nodes[0]

def resolve_phandle(dt, phandle):
    p = node_by_phandle(dt, phandle)
    return os.path.join(p.path, p.name)

def add_overlay(overlay, path):
    for f in range(100):
        nodename = 'fragment@' + str(f)
        if overlay.exist_node(nodename):
            pass
        else:
            fragnode = fdt.Node(nodename)
            if path[0] == '&':
                fragnode.set_property('target', 0xffffffff)
                overlay.set_property(path[1:], '/'+nodename+':target:0', path='/__fixups__')
            else:
                fragnode.set_property('target-path', path)
            overlay.add_item(fragnode)
            ovlnode = fdt.Node('__overlay__')
            fragnode.append(ovlnode)
            return ovlnode

def add_fixup(overlay, label, fixup_path):
    if overlay.exist_node('__fixups__'):
        fixups = overlay.get_node('__fixups__')
    else:
        fixups = fdt.Node('__fixups__')
        overlay.add_item(fixups)
    prev_paths = fixups.get_property(label)
    if prev_paths:
        fixups.set_property(label, prev_paths.data + [fixup_path])
    else:
        fixups.set_property(label, [fixup_path])

def add_local_fixup(overlay, parent_path, name):
    overlay.set_property(name, 0, path='__local_fixups__'+parent_path)

def add_gpio_vol_keys(dt, overlay, gpio_keys_ovl):
    symbols = dt.get_node('__symbols__')

    keydata = dt.get_node('/play_joystick').get_property('key-gpios').data
    pindata = dt.get_node('/pinctrl/buttons/gpio-key-pin').get_property('rockchip,pins').data
    # extract volume keys from array
    vol_up = keydata[14*3:15*3]
    vol_up_pin = pindata[14*4:15*4]
    vol_dn = keydata[15*3:16*3]
    vol_dn_pin = pindata[15*4:16*4]

    gpio_path = resolve_phandle(dt, vol_up[0])
    gpio_sym = [p.name for p in symbols.props if p.value == gpio_path][0]
    gpio_phandle = 0xffffffff

    pins_path = gpio_keys_ovl.path+'/__overlay__/pinctrl/btns/btn-pins-vol-overlay'
    keys_path = gpio_keys_ovl.path+'/__overlay__/gpio-keys-overlay'

    pullup = 0xffffffff
    overlay.set_property('rockchip,pins', vol_up_pin[0:3] + [pullup] + vol_dn_pin[0:3] + [pullup], path=pins_path)
    pins_phandle = overlay.add_label('overlay_btns')
    overlay.set_property('phandle', pins_phandle, path=pins_path)
    add_fixup(overlay, 'pcfg_pull_up', pins_path+':rockchip,pins:12')
    add_fixup(overlay, 'pcfg_pull_up', pins_path+':rockchip,pins:28')

    overlay.set_property('compatible', 'gpio-keys', path=keys_path)
    overlay.set_property('autorepeat', None, path=keys_path)
    overlay.set_property('pinctrl-0', pins_phandle, path=keys_path)
    add_local_fixup(overlay, keys_path, 'pinctrl-0')
    overlay.set_property('pinctrl-names', 'default', path=keys_path)

    overlay.set_property('gpios', [gpio_phandle, vol_up[1], vol_up[2]], path=keys_path+'/button-vol-up')
    overlay.set_property('label', "VOLUMEUP", path=keys_path+'/button-vol-up')
    overlay.set_property('linux,code', 0x73, path=keys_path+'/button-vol-up')
    add_fixup(overlay, gpio_sym, keys_path+'/button-vol-up:gpios:0')

    overlay.set_property('gpios', [gpio_phandle, vol_dn[1], vol_dn[2]], path=keys_path+'/button-vol-down')
    overlay.set_property('label', "VOLUMEDOWN", path=keys_path+'/button-vol-down')
    overlay.set_property('linux,code', 0x72, path=keys_path+'/button-vol-down')
    add_fixup(overlay, gpio_sym, keys_path+'/button-vol-down:gpios:0')


def switch_joypad_to_mymini(overlay, jp_ovl):
    # My mini has separate ADC channels for axis, so we need another driver
    jp_ovl.set_property('compatible', "archr-joypad")
    jp_ovl.set_property('io-channel-names', ["key-RY", "key-RX", "key-LY", "key-LX"])
    jp_ovl.set_property('io-channels', [0xffffffff, 3, 0xffffffff, 3, 0xffffffff, 2, 0xffffffff, 1])
    add_fixup(overlay, 'saradc', jp_ovl.path+'/__overlay__:io-channels:0')
    add_fixup(overlay, 'saradc', jp_ovl.path+'/__overlay__:io-channels:8')
    add_fixup(overlay, 'saradc', jp_ovl.path+'/__overlay__:io-channels:16')
    add_fixup(overlay, 'saradc', jp_ovl.path+'/__overlay__:io-channels:24')
    jp_ovl.set_property('button-adc-scale', 2)
    jp_ovl.set_property('button-adc-deadzone', 216)
    jp_ovl.set_property('button-adc-fuzz', 54)
    jp_ovl.set_property('button-adc-flat', 54)
    jp_ovl.set_property('abs_x-p-tuning', 180)
    jp_ovl.set_property('abs_x-n-tuning', 180)
    jp_ovl.set_property('abs_y-p-tuning', 180)
    jp_ovl.set_property('abs_y-n-tuning', 180)
    jp_ovl.set_property('abs_rx-p-tuning', 0)
    jp_ovl.set_property('abs_rx-n-tuning', 0)
    jp_ovl.set_property('abs_ry-p-tuning', 0)
    jp_ovl.set_property('abs_ry-n-tuning', 0)
    jp_ovl.set_property('poll-interval', 10)


def copy_gpio_prop(dt, symbols, overlay, src_node, prop_name, ovl_target_path):
    """Propagate a *-gpios property from `src_node` (vendor DTB) into the
    overlay at `ovl_target_path`, attaching the correct __fixup__ so the
    bootloader resolves the phandle against the live base DTB.

    Returns True on success, False if the property is absent or the
    referenced controller cannot be resolved. Logs nothing — caller can
    decide whether to warn.
    """
    if not src_node.exist_property(prop_name):
        return False
    try:
        data = src_node.get_property(prop_name).data
    except Exception:
        return False
    if not data or len(data) < 3:
        return False
    try:
        gpio_path = resolve_phandle(dt, data[0])
        gpio_sym = next(p.name for p in symbols.props if p.value == gpio_path)
    except (StopIteration, Exception):
        return False
    overlay.set_property(prop_name, [0xffffffff, data[1], data[2]],
                         path=ovl_target_path)
    add_fixup(overlay, gpio_sym, ovl_target_path + ':' + prop_name + ':0')
    return True


def extract_joypad_wiring(dt, overlay, jp_ovl, args, symbols):
    """Propagate vendor-specific joypad wiring to the overlay:
      * amux-a-gpios, amux-b-gpios, amux-en-gpios  (root joypad node)
      * gpios on each swN child (per-button)
      * button-adc-{deadzone,fuzz,flat} when vendor differs from defaults

    These vary per board family: clone uses pins 16/15/11, original
    uses 11/8/13, soysauce uses 13/12/17. The button GPIOs vary even
    more (different banks). Without this propagation the merged DTB
    keeps our base's "original" wiring and clone/soysauce hardware
    boots with all buttons mapped to wrong pins.
    """
    try:
        jp = dt.get_node('/odroidgo3-joypad')
    except Exception:
        return

    ovl_root = jp_ovl.path + '/__overlay__'

    # amux selectors
    for prop in ('amux-a-gpios', 'amux-b-gpios', 'amux-en-gpios'):
        if copy_gpio_prop(dt, symbols, overlay, jp, prop, ovl_root):
            data = jp.get_property(prop).data
            args['logger'].info(f"{prop} pin={data[1]} flags={data[2]} on {ovl_root}")

    # per-button gpios (sw1..sw22 etc.)
    button_count = 0
    for sw_node in jp.nodes:
        name = sw_node.name
        if not name.startswith('sw'):
            continue
        target = ovl_root + '/' + name
        if copy_gpio_prop(dt, symbols, overlay, sw_node, 'gpios', target):
            button_count += 1
    if button_count:
        args['logger'].info(f"propagated {button_count} per-button gpios on {ovl_root}")

    # button-adc tunings — only emit override if vendor differs from
    # the default our base ships. Saves overlay size for the common case.
    BUTTON_ADC_DEFAULTS = {
        'button-adc-deadzone': 96,
        'button-adc-fuzz':     48,
        'button-adc-flat':     48,
    }
    for prop, default in BUTTON_ADC_DEFAULTS.items():
        if not jp.exist_property(prop):
            continue
        val = jp.get_property(prop).value
        if val != default:
            overlay.set_property(prop, val, path=ovl_root)
            args['logger'].info(f"{prop}={val} (vendor differs from default {default})")

    # rumble-gpio: some clone boards (R36S V20 2025-05-18 batch, GR36)
    # expose a dedicated GPIO that gates the rumble motor — without
    # propagating it the motor sits in its default state, which on
    # those boards is "on" (= constant vibration). The audit across
    # 44 vendor DTBs flagged this in Tier 3; we handle it explicitly
    # here so the overlay carries the right pin and the bootloader
    # phandle fixup resolves against our GPIO controller.
    if copy_gpio_prop(dt, symbols, overlay, jp, 'rumble-gpio', ovl_root):
        data = jp.get_property('rumble-gpio').data
        args['logger'].info(f"rumble-gpio pin={data[1]} flags={data[2]} on {ovl_root}")


# ---------- Tier 3: auto-discovery of custom vendor properties --------------
#
# Some vendor BSPs ship board-specific properties we never planned for
# (rumble-direction-fix, special-button-quirk, custom-trigger-mode, ...).
# The "extract every known prop" approach leaves those on the cutting
# room floor. Tier 3 walks the vendor tree, identifies anything we don't
# already handle, and propagates it to the overlay — with logging so
# the user knows exactly which non-standard props came along.

# Props we ALREADY handle via dedicated extraction logic above; don't
# touch them in the auto-discovery pass.
_KNOWN_PANEL_PROPS = frozenset({
    'compatible', 'reg', 'phandle', 'linux,phandle', 'status',
    'panel_description', 'panel-init-sequence', 'panel-exit-sequence',
    'display-timings', 'rotation',
    'dsi,format', 'dsi,lanes', 'dsi,flags', 'dsi,burst-mode', 'dsi,sync-mode',
    'prepare-delay-ms', 'reset-delay-ms', 'init-delay-ms',
    'enable-delay-ms', 'disable-delay-ms', 'unprepare-delay-ms',
    'width-mm', 'height-mm', 'bus-format', 'bpc',
    'reset-gpios', 'enable-gpios',
    'led-red-gpios', 'led-green-gpios',
    'led-blue1-gpios', 'led-blue2-gpios',
    'iovcc-supply', 'vdd-supply', 'vsp-supply', 'vsn-supply',
    'vcc-supply', 'power-supply', 'backlight', 'backlight-supply',
})

_KNOWN_JOYPAD_PROPS = frozenset({
    'compatible', 'reg', 'phandle', 'linux,phandle', 'status',
    'pinctrl-names', 'pinctrl-0', 'pinctrl-1',
    'io-channel-names', 'io-channels',
    'amux-count', 'amux-channel-mapping',
    'amux-a-gpios', 'amux-b-gpios', 'amux-en-gpios',
    'button-adc-deadzone', 'button-adc-fuzz', 'button-adc-flat',
    'button-adc-scale', 'button-adc-tuned-scale',
    'abs_x-p-tuning', 'abs_x-n-tuning', 'abs_y-p-tuning', 'abs_y-n-tuning',
    'abs_rx-p-tuning', 'abs_rx-n-tuning', 'abs_ry-p-tuning', 'abs_ry-n-tuning',
    'abs-x-p-tuned-val', 'abs-x-n-tuned-val',
    'abs-y-p-tuned-val', 'abs-y-n-tuned-val',
    'abs-rx-p-tuned-val', 'abs-rx-n-tuned-val',
    'abs-ry-p-tuned-val', 'abs-ry-n-tuned-val',
    'abs-left-first', 'poll-interval',
    'invert-absx', 'invert-absy', 'invert-absrx', 'invert-absry',
    'swap-absxy', 'swap-absrxy', 'vendor-direction',
    'rumble-boost-weak', 'rumble-boost-strong',
    'rumble-gpio',
    'pwms', 'pwm-names',
    'key-gpios',
})

_KNOWN_SW_PROPS = frozenset({
    'gpios', 'phandle', 'linux,phandle',
    'amux-channel',
})

# Props that must NEVER be propagated from vendor, even if discovered.
# Either userspace expects our specific value (joypad-name, linux,code)
# or our base intentionally diverges (regulator ranges for OC).
_BLACKLIST = frozenset({
    'joypad-name', 'joypad-product', 'joypad-revision',
    'linux,code', 'label',
    'regulator-min-microvolt', 'regulator-max-microvolt',
    'regulator-ramp-delay', 'regulator-initial-mode',
    'regulator-always-on', 'regulator-boot-on',
})


def _prop_is_phandle_array(prop):
    """Heuristic: PropWords with values that look like phandles (small
    positive integers, multiple entries). Without knowing the exact
    layout (#xxx-cells) we can't remap; skip these and let the user
    add manual support if needed."""
    if not isinstance(prop, fdt.PropWords):
        return False
    # Common phandle arrays have 2-4 cells per entry; single-cell
    # numerics (delays, sizes) are safe to copy.
    if len(prop.data) <= 1:
        return False
    # If every value is small (< 0x1000), it's likely a numeric tuning
    # array; if any is "large" (>= 0x10), it's likely a phandle. Soft
    # rule but works for the props we've seen in the wild.
    return any(v >= 0x10 for v in prop.data)


def _set_prop_on_overlay(overlay, prop, ovl_target_path):
    """Copy a Property to the overlay tree at the given path. Handles
    the three concrete subclasses of fdt.Property we care about."""
    if isinstance(prop, fdt.PropStrings):
        overlay.set_property(prop.name, prop.data, path=ovl_target_path)
    elif isinstance(prop, fdt.PropWords):
        overlay.set_property(prop.name, prop.data, path=ovl_target_path)
    elif isinstance(prop, fdt.PropBytes):
        overlay.set_property(prop.name, bytes(prop.data), path=ovl_target_path)
    elif hasattr(prop, 'value') and prop.value is None:
        # Boolean-style empty property
        overlay.set_property(prop.name, True, path=ovl_target_path)
    else:
        # Unknown type — skip rather than crash
        return False
    return True


def auto_discover_custom(dt, overlay, panel_ovl, jp_ovl, args):
    """Walk panel + joypad + sw* subnodes in the vendor DTB. Any prop
    not in our known/handled set and not blacklisted gets propagated
    to the overlay. Phandle arrays are skipped (would need remap that
    we don't have layout info for). Logs each discovery."""
    discoveries = []

    def _visit(node, known_set, ovl_target_path, scope_label):
        for p in node.props:
            if p.name in known_set or p.name in _BLACKLIST:
                continue
            if _prop_is_phandle_array(p):
                args['logger'].info(
                    f"auto-discovery: skip {scope_label}/{p.name} "
                    f"(phandle array, layout unknown)")
                continue
            if _set_prop_on_overlay(overlay, p, ovl_target_path):
                val = p.value if hasattr(p, 'value') else '<binary>'
                discoveries.append((scope_label, p.name, val))

    # Panel
    try:
        panel = dt.get_node('/dsi@ff450000/panel@0')
        panel_ovl_path = panel_ovl.path + '/__overlay__/dsi@ff450000/panel@0'
        _visit(panel, _KNOWN_PANEL_PROPS, panel_ovl_path, 'panel')
    except Exception:
        pass

    # Joypad root
    try:
        jp = dt.get_node('/odroidgo3-joypad')
        jp_ovl_path = jp_ovl.path + '/__overlay__'
        _visit(jp, _KNOWN_JOYPAD_PROPS, jp_ovl_path, 'joypad')
        # And each sw* subnode
        for sw in jp.nodes:
            if not sw.name.startswith('sw'):
                continue
            _visit(sw, _KNOWN_SW_PROPS, jp_ovl_path + '/' + sw.name,
                   f'joypad/{sw.name}')
    except Exception:
        pass

    if discoveries:
        args['logger'].info(
            f"auto-discovery: propagated {len(discoveries)} custom vendor props")
        for scope, name, val in discoveries:
            args['logger'].info(f"  custom: {scope}/{name} = {val!r}")
    return discoveries


def make_dtbo(dtb_data, args):
    dt = fdt.parse_dtb(dtb_data)
    symbols = dt.get_node('__symbols__')

    dsipath = symbols.get_property('dsi').value
    # panelpath is /dsi@ff450000/panel@0 on rk3326 and /dsi@fe060000/panel@0 on rk3566
    panelpath = dsipath + '/panel@0'
    panel = dt.get_node(panelpath)
    pdesc = panel_to_desc(panel, args)

    # remove empty lines as pyfdt does not like them
    pdesc = [ l for l in pdesc if l != '']

    panel_rst_gpio = panel.get_property('reset-gpios').data
    gpio_sym = [p.name for p in symbols.props if p.value == resolve_phandle(dt, panel_rst_gpio[0])][0]
    gpio_num = int(gpio_sym[4:])

    # create an overlay tree
    overlay = fdt.FDT()
    overlay.header.version = 17

    panel_ovl = add_overlay(overlay, '/')
    panel_ovl_path = panel_ovl.path+'/__overlay__'+panelpath
    overlay.set_property('compatible', 'archr,generic-dsi', path=panel_ovl_path)
    overlay.set_property('panel_description', pdesc, path=panel_ovl_path)
    if 'DR90' in args['flags']:
        overlay.set_property('rotation', 90, path=panel_ovl_path)
    elif 'DR180' in args['flags']:
        overlay.set_property('rotation', 180, path=panel_ovl_path)
    elif 'DR270' in args['flags']:
        overlay.set_property('rotation', 270, path=panel_ovl_path)

    compat = dt.get_node('/').get_property('compatible').data[0]
    args['logger'].info(f"compatible {compat}")
    # Original-only short-circuit: the original kernel DTB (rk3326-odroid-go2)
    # already has the correct reset-gpios, power-supply and backlight wiring,
    # so the DTBO only needs panel description/timings/init-sequence.
    #
    # Gate by subdevice (passed as 'SDORIG' from generator.sh), NOT by vendor
    # compatible alone: several clone vendor DTBs (e.g. G80CA V1.2 04-22/04-23
    # Panel 8 and 9, G80C V1.1 Panel 9) ship with
    # `compatible = "rockchip,rk3326-odroidgo3-linux"` despite being clone
    # hardware. Trusting that string skipped the GPIO overrides on clones and
    # produced the "backlight on, no image" black screen on those boards.
    if 'SDORIG' in args['flags'] and 'odroidgo3' in compat:
        return overlay.to_dtb()

    # copy reset config
    pins_path = panel_ovl.path+'/__overlay__/pinctrl/gpio-lcd/lcd-rst'
    overlay.set_property('reset-gpios', [0xffffffff, panel_rst_gpio[1], panel_rst_gpio[2]], path=panel_ovl_path)
    add_fixup(overlay, gpio_sym, panel_ovl_path+':reset-gpios:0')
    overlay.set_property('rockchip,pins', [gpio_num, panel_rst_gpio[1], 0, 0xffffffff], path=pins_path)
    add_fixup(overlay, 'pcfg_pull_none', pins_path+':rockchip,pins:12')
    # power supply gpio fetch (some trees do not have power-supply prop)
    try:
        panel_ps = dt.get_node(resolve_phandle(dt, panel.get_property('power-supply').value))
        panel_ps_gpio = panel_ps.get_property('gpio').data
        gpio_sym = [p.name for p in symbols.props if p.value == resolve_phandle(dt, panel_ps_gpio[0])][0]
        gpio_num = int(gpio_sym[4:])
        # power supply reg
        panel_reg_path = panel_ovl.path+'/__overlay__/vcc18-lcd0'
        overlay.set_property('gpio', [0xffffffff, panel_ps_gpio[1], panel_ps_gpio[2]], path=panel_reg_path)
        add_fixup(overlay, gpio_sym, panel_reg_path+':gpio:0')
        # power supply pinctrl
        panel_ps_pin_path = panel_ovl.path+'/__overlay__/pinctrl/vcc18-lcd/vcc18-lcd-n'
        overlay.set_property('rockchip,pins', [gpio_num, panel_ps_gpio[1], 0, 0xffffffff], path=panel_ps_pin_path)
        add_fixup(overlay, 'pcfg_pull_none', panel_ps_pin_path+':rockchip,pins:12')
    except:
        pass
    # some devices (e.g. R36s clone) have enable-gpios defined
    try:
        panel_en_gpio = panel.get_property('enable-gpios').data
        gpio_sym = [p.name for p in symbols.props if p.value == resolve_phandle(dt, panel_en_gpio[0])][0]
        overlay.set_property('enable-gpios', [0xffffffff, panel_en_gpio[1], panel_en_gpio[2]], path=panel_ovl_path)
        add_fixup(overlay, gpio_sym, panel_ovl_path+':enable-gpios:0')
    except:
        pass

    # Soysauce Y3506 V04+/V05 boards have status LEDs on the panel
    # itself (red + blue1/blue2/green) used for battery/charging
    # indication. Tier 3 audit flagged these in 8 vendor DTBs. Without
    # propagation the LED GPIOs default to the bootloader state — on
    # most boards they end up "on", giving the user a permanent red
    # light that doesn't react to charging state. Each is a standard
    # <phandle pin flags> triple, same layout as reset-gpios.
    for led_prop in ('led-red-gpios', 'led-green-gpios',
                     'led-blue1-gpios', 'led-blue2-gpios'):
        if copy_gpio_prop(dt, symbols, overlay, panel, led_prop, panel_ovl_path):
            data = panel.get_property(led_prop).data
            args['logger'].info(f"panel {led_prop} pin={data[1]} flags={data[2]}")


    # If the vendor DTB explicitly disables /adc-keys, mirror that decision
    # in the overlay. We deliberately do NOT treat "/adc-keys absent" as
    # MyMini hardware: most ArchR vendor DTBs are partial extractions that
    # don't ship that node even when the underlying board has K36-style
    # single-ADC sticks. Auto-detecting MyMini from absence broke input on
    # every K36 R36S in our 43-board set. Default is K36 unless JPmm is
    # explicitly requested or /adc-keys is present-but-disabled.
    need_adckeys_disable = False
    if dt.exist_node('/adc-keys'):
        adckeys_orig = dt.get_node('/adc-keys')
        adckeys_status = adckeys_orig.get_property('status')
        if (adckeys_status) and (adckeys_status.value == 'disabled'):
            need_adckeys_disable = True
    if need_adckeys_disable:
        noadck_ovl = add_overlay(overlay, '/')
        overlay.set_property('dtbo_comment', 'deliberately-disabled-adc-keys', path=noadck_ovl.path+'/__overlay__/adc-keys')
        overlay.set_property('status', 'disabled', path=noadck_ovl.path+'/__overlay__/adc-keys')
        args['logger'].info(f"disabled adc-keys")
        # TODO: extract GPIO keys from play_joystick.key-gpios[14..15]
        if dt.exist_node('/play_joystick'):
            add_gpio_vol_keys(dt, overlay, noadck_ovl)
    else:
        adck_ovl = add_overlay(overlay, '/')
        overlay.set_property('status', 'okay', path=adck_ovl.path+'/__overlay__/adc-keys')


    # joypad overlay always present to simplify different options
    jp_ovl = add_overlay(overlay, '&joypad')

    # If not explicitly specified, disabled adc-keys imply multi-adc MyMini-style joypad
    if 'JPk36' in args['flags']:
        pass
    elif ('JPmm' in args['flags']) or (need_adckeys_disable):
        args['logger'].info(f"My Mini joypad tweaks on {jp_ovl.path}")
        switch_joypad_to_mymini(overlay, jp_ovl)

    # Left stick was inverted by default, so invert inversion here
    if ('LSi' not in args['flags']) ^ (need_adckeys_disable):
        jp_ovl.set_property('invert-absx', 1)
        jp_ovl.set_property('invert-absy', 1)
        args['logger'].info(f"left stick un-inverted on {jp_ovl.path}")
    else:
        args['logger'].info(f"left stick left inverted")

    # If needed, invert right stick
    if 'RSi' in args['flags']:
        jp_ovl.set_property('invert-absrx', 1)
        jp_ovl.set_property('invert-absry', 1)
        args['logger'].info(f"invert right stick on {jp_ovl.path}")

    # Propagate the vendor's joypad hardware wiring (amux pins, per-button
    # GPIOs, ADC tunings). Without this, clone and soysauce boards inherit
    # the "original" base layout and end up with controls all over the
    # place — see audit_all.py for the pin matrix across the 44 vendor
    # DTBs we ship.
    extract_joypad_wiring(dt, overlay, jp_ovl, args, symbols)


    try:
        snd = dt.get_node('/rk817-sound')

        # fetch raw   hp-det-gpio = <0x6f 0x16 0x00>;
        hpdet = snd.get_property('hp-det-gpio').data
        hp_det = dt.get_node('/pinctrl/headphone/hp-det')
        hp_det_pins = hp_det.get_property('rockchip,pins')
        hp_det_pull_node = node_by_phandle(dt, hp_det_pins[3])
        # resolve <0x6f> into '/pinctrl/gpio2@ff260000'
        hpdet_gpio_path = resolve_phandle(dt, hpdet[0])
        # find symbol 'gpio2' for path '/pinctrl/gpio2@ff260000'
        gpiosyms = [p.name for p in symbols.props if p.value == hpdet_gpio_path]
        gpio_sym = gpiosyms[0]

        # on success, add overlay
        hpdet_ovl = add_overlay(overlay, '/')

        force_simple_audio_routing = ('SRs' in args['flags'])

        # Detect amplifier GPIO. Direct `spk-con-gpio` on the sound node is
        # the legacy path. Newer R36S clones (post-2025-03-18 batches) wire
        # the amplifier through `rockchip,codec/spk-ctl-gpios` instead, so
        # we follow the codec phandle as a fallback.
        amp_gpio = None
        if not force_simple_audio_routing:
            if snd.exist_property('spk-con-gpio'):
                amp_gpio = snd.get_property('spk-con-gpio').data
            elif snd.exist_property('rockchip,codec'):
                snd_codec_ph = snd.get_property('rockchip,codec').value
                snd_codec_path = resolve_phandle(dt, snd_codec_ph)
                snd_codec = dt.get_node(snd_codec_path)
                if snd_codec.exist_property('spk-ctl-gpios'):
                    amp_gpio = snd_codec.get_property('spk-ctl-gpios').data

        # Determine preset, configure amplifier if needed
        if amp_gpio:
            rk817_path = hpdet_ovl.path+'/__overlay__/rk817-sound-amplified'
            amp_gpio_sym = [p.name for p in symbols.props if p.value == resolve_phandle(dt, amp_gpio[0])][0]
            gpio_num = int(amp_gpio_sym[4:])
            args['logger'].info(f"spk-con-gpio {amp_gpio_sym} {amp_gpio[1]} on {hpdet_ovl.path}")

            amp_path = hpdet_ovl.path+'/__overlay__/audio-amplifier'
            overlay.set_property('enable-gpios', [0xffffffff, amp_gpio[1], amp_gpio[2]], path=amp_path)
            add_fixup(overlay, amp_gpio_sym, amp_path+':enable-gpios:0')

            amp_pc_path = hpdet_ovl.path+'/__overlay__/pinctrl/speaker/spk-amp-enable-h'
            overlay.set_property('rockchip,pins', [gpio_num, amp_gpio[1], 0, 0xffffffff], path=amp_pc_path)
            add_fixup(overlay, 'pcfg_pull_none', amp_pc_path+':rockchip,pins:12')
        else:
            rk817_path = hpdet_ovl.path+'/__overlay__/rk817-sound-simple'

        overlay.set_property('status', 'okay', path=rk817_path)
        # for some reason hp detection polarity needs to be inverted on some devices
        if 'HPi' in args['flags']:
            if hpdet[2] == 0:
                hpdet[2] = 1
            else:
                hpdet[2] = 0
        overlay.set_property('simple-audio-card,hp-det-gpio', [0xffffffff, hpdet[1], hpdet[2]], path=rk817_path)
        add_fixup(overlay, gpio_sym, rk817_path+':simple-audio-card,hp-det-gpio:0')
        pins_path = hpdet_ovl.path+'/__overlay__/pinctrl/headphone/hp-det'
        overlay.set_property('rockchip,pins', hp_det_pins[0:3] + [0xffffffff], path=pins_path)
        # Restore bias reference
        if hp_det_pull_node.exist_property('bias-pull-down'):
            add_fixup(overlay, 'pcfg_pull_down', pins_path+':rockchip,pins:12')
        else:
            add_fixup(overlay, 'pcfg_pull_up', pins_path+':rockchip,pins:12')
        args['logger'].info(f"hp-det-gpio {gpiosyms[0]} on {hpdet_ovl.path}")

    except Exception as e:
        args['logger'].info(e)

    # Tier 3: walk any remaining vendor properties we haven't already
    # handled and propagate them. Catches board-specific quirks (custom
    # rumble flags, exotic input properties, etc.) without us needing
    # to hardcode every possible vendor invention.
    auto_discover_custom(dt, overlay, panel_ovl, jp_ovl, args)

    # Move fixups to the very end (if any)
    if overlay.exist_node('__fixups__'):
        fixups = overlay.get_node('__fixups__')
        overlay.remove_node('__fixups__')
        overlay.add_item(fixups)

    if overlay.exist_node('__local_fixups__'):
        fixups = overlay.get_node('__local_fixups__')
        overlay.remove_node('__local_fixups__')
        overlay.add_item(fixups)

    # send the overlay to output
    return overlay.to_dtb()


if __name__ == "__main__":
    import argparse, logging
    logging.basicConfig(level=logging.INFO)
    logger = logging.getLogger("dtbo")

    parser = argparse.ArgumentParser(description="Generate a dtbo from a stock dtb")
    parser.add_argument(dest="src", help="input stock dtb path", metavar="/path/to/stock.dtb")
    parser.add_argument(dest="opts", help="dtbo options", nargs='?', metavar="LSi-HPi", default="")
    parser.add_argument("-o", "--output", help="output (dtbo) path")
    args = parser.parse_args()

    with open(args.src, 'rb') as f:
        content = f.read()

    flags = args.opts.split('-')
    flags = [ f for f in flags if f != '']
    dtbo = make_dtbo(content, {'flags': flags, 'logger': logger})

    if args.output:
        with open(args.output, 'wb') as f:
            f.write(dtbo)
    else:
        sys.stdout.buffer.write(dtbo)
