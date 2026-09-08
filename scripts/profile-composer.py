#!/usr/bin/env python3
"""Profile a native composer on a dedicated Linux/X11 display.

Requires xdotool, xclip, ImageMagick, and an isolated mock daemon with two
seeded chats. Pass that fixture's UI settings and IPC port. Never sends a turn.
CPU percentages are relative to one core; the UI process includes GPU workers.
"""
import subprocess, os, pathlib, time, json, threading, hashlib, argparse, shutil, re
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('binary', type=pathlib.Path)
parser.add_argument('output', type=pathlib.Path)
parser.add_argument('--settings', required=True, type=pathlib.Path)
parser.add_argument('--ipc-port', required=True)
parser.add_argument('--display', required=True)
args = parser.parse_args()
out = args.output.resolve()
out.mkdir(parents=True, exist_ok=False)
name = out.name
binary = out / 'profiled-zeron'
shutil.copy2(args.binary, binary)
settings = json.loads(args.settings.read_text())
(out / 'ui-settings.json').write_text(json.dumps(settings))
env = os.environ | {'DISPLAY': args.display, 'WAYLAND_DISPLAY': '', 'ZERON_IPC_PORT': args.ipc_port, 'ZERON_HARNESS': 'mock', 'ZERON_WORKOS_CLIENT_ID': '', 'ZERON_EDGE_URL': 'http://127.0.0.1:1', 'ZERON_DATA_DIR': str(out), 'RUST_LOG': 'warn', 'LP_NUM_THREADS': '4'}
p = subprocess.Popen([str(binary)], env=env, stdout=open(out / 'ui.log', 'w'), stderr=subprocess.STDOUT, start_new_session=True)
hz = os.sysconf('SC_CLK_TCK')

def stat():
    s = pathlib.Path(f'/proc/{p.pid}/stat').read_text().split(') ')[1].split()
    m = pathlib.Path(f'/proc/{p.pid}/task/{p.pid}/stat').read_text().split(') ')[1].split()
    return {'cpu': (int(s[11]) + int(s[12])) / hz, 'main': (int(m[11]) + int(m[12])) / hz, 'rss': int(s[21]) * os.sysconf('SC_PAGE_SIZE') / 1048576}

def key(*a):
    subprocess.run(['xdotool', *a], env=env, check=True, stdout=subprocess.DEVNULL)

def paste(text):
    subprocess.run(['xclip', '-selection', 'clipboard'], input=text.encode(), env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    key('key', 'ctrl+a', 'ctrl+v')

def idle():
    time.sleep(5)

def typing():
    for i in range(16):
        key('type', '--clearmodifiers', '--delay', '5', f'Line {i:02d}: Measuring native composer layout and resizing performance.')
        key('key', 'shift+Return')
        time.sleep(0.12)
    time.sleep(0.4)

def scroll():
    key('mousemove', '720', '660')
    for i in range(48):
        key('click', '4' if i // 8 % 2 == 0 else '5')
        time.sleep(0.08)
    time.sleep(0.3)

def edits():
    key('key', 'ctrl+a', 'BackSpace')
    time.sleep(0.4)
    for i in range(6):
        paste('A draft that grows and shrinks while keeping its input anchored.\n' * (3 if i % 2 == 0 else 9))
        time.sleep(0.45)

def large_paste():
    for i in range(3):
        paste(f'Pass {i}: A long draft exercises wrapping and cached layout during repaint.\n' * 1000)
        time.sleep(0.6)
results = []

def phase(name, fn):
    stop = threading.Event()
    samples = []

    def sample():
        while not stop.wait(0.1):
            try:
                samples.append(stat())
            except FileNotFoundError:
                break
    t = threading.Thread(target=sample)
    t.start()
    start = stat()
    when = time.monotonic()
    fn()
    end = stat()
    duration = time.monotonic() - when
    stop.set()
    t.join()
    result = {'phase': name, 'seconds': duration, 'cpuPercent': 100 * (end['cpu'] - start['cpu']) / duration, 'mainCpuPercent': 100 * (end['main'] - start['main']) / duration, 'peakRssMiB': max((x['rss'] for x in samples + [start, end])), 'endRssMiB': end['rss']}
    results.append(result)
    print(json.dumps(result), flush=True)
try:
    time.sleep(2)
    ids = subprocess.check_output(['xdotool', 'search', '--class', 'zeron'], env=env, text=True).splitlines()
    wid = ids[-1]
    key('windowmap', wid, 'windowsize', wid, '1280', '900', 'windowmove', wid, '0', '0', 'windowfocus', wid)
    time.sleep(1)
    key('mousemove', '110', '160', 'click', '1')
    time.sleep(0.5)
    key('mousemove', '500', '830', 'click', '1')
    key('key', 'ctrl+a', 'BackSpace')
    time.sleep(1)
    phase('idle_focused', idle)
    phase('typing_resize', typing)
    phase('scroll', scroll)
    phase('paste_resize', edits)
    phase('large_paste', large_paste)
    phase('large_scroll', scroll)
    phase('large_idle_focused', idle)
    root = re.search('Window id: (0x[0-9a-f]+)', subprocess.check_output(['xwininfo', '-root'], env=env, text=True))[1]
    key('windowfocus', root)
    time.sleep(0.5)
    phase('idle_unfocused', idle)
    subprocess.run(['import', '-window', 'root', str(out / 'final.png')], env=env, check=True)
    (out / 'results.json').write_text(json.dumps({'name': name, 'binarySha256': hashlib.sha256(binary.read_bytes()).hexdigest(), 'phases': results}, indent=2))
finally:
    p.terminate()
    p.wait(timeout=10)
