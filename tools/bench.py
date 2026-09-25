"""Runs the benchmark matrix: servers x scenarios x viewer profiles."""
import argparse, subprocess, time, os, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REL = os.path.join(ROOT, 'target', 'release')

def run_case(addr, profile, scenario, secs, label, pid, origin, extra_app=(), verify=False):
    app = subprocess.Popen([os.path.join(REL, 'praeter-testapp.exe'), '--scenario', scenario, '--secs', str(secs + 3.5),
                            '--hold', '4' if verify else '0', *extra_app],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(2.0)
    cmd = [os.path.join(REL, 'praeter-bench.exe'), '--addr', addr, '--profile', profile, '--secs', str(secs),
           '--label', label, '--origin', origin]
    if pid:
        cmd += ['--pid', str(pid)]
    if verify:
        cmd += ['--verify']
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=secs + 60)
    app.wait(timeout=60)
    out = r.stdout.strip() or ('ERR ' + r.stderr.strip().splitlines()[-1] if r.stderr.strip() else 'ERR')
    return out

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--servers', default='praeter,tightvnc')
    ap.add_argument('--scenarios', default='ticker,typing,scroll,drag,video')
    ap.add_argument('--profiles', default='tightvnc')
    ap.add_argument('--secs', type=int, default=8)
    ap.add_argument('--praeter-args', default='')
    ap.add_argument('--tvn-pid', type=int, default=0)
    ap.add_argument('--verify', action='store_true')
    ap.add_argument('--rtt', type=float, default=0, help='emulated RTT in ms via praeter-netem')
    ap.add_argument('--mbit', type=float, default=0, help='emulated bandwidth cap')
    a = ap.parse_args()
    srv = None
    servers = {}
    if 'praeter' in a.servers:
        log = open(os.path.join(ROOT, 'target', 'praeter-bench-server.log'), 'w')
        srv = subprocess.Popen([os.path.join(REL, 'praetervnc.exe'), '--port', '5905', '--bind', '127.0.0.1', *a.praeter_args.split()],
                               stdout=log, stderr=log)
        time.sleep(1.0)
        servers['praeter'] = ('127.0.0.1:5905', srv.pid)
    if 'tightvnc' in a.servers:
        servers['tightvnc'] = ('127.0.0.1:5911', a.tvn_pid)
    proxies = []
    if a.rtt > 0 or a.mbit > 0:
        for i, (k, (addr, pid)) in enumerate(list(servers.items())):
            port = 5930 + i
            args = [os.path.join(REL, 'praeter-netem.exe'), str(port), addr, str(a.rtt / 2)]
            if a.mbit > 0:
                args.append(str(a.mbit))
            proxies.append(subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
            servers[k] = (f'127.0.0.1:{port}', pid)
        time.sleep(0.5)
    try:
        for sc in a.scenarios.split(','):
            for prof in a.profiles.split(','):
                for name in a.servers.split(','):
                    addr, pid = servers[name]
                    tag = f'{name[:7]}/{sc}/{prof[:8]}' + (f'/rtt{a.rtt:g}' if a.rtt else '') + (f'/{a.mbit:g}M' if a.mbit else '')
                    print(run_case(addr, prof, sc, a.secs, tag, pid, '0,0', verify=a.verify), flush=True)
    finally:
        if srv:
            srv.kill()
        for p in proxies:
            p.kill()

if __name__ == '__main__':
    main()
