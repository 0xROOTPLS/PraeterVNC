"""Builds dist/: the MSI, the portable exe, license files and SHA256SUMS.txt."""
import glob, hashlib, os, re, shutil, subprocess, sys, tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIST = os.path.join(ROOT, 'dist')
TARGET = os.path.join(ROOT, 'target', 'package')


def run(*a, env=None):
    print('>', ' '.join(a))
    subprocess.run(a, cwd=ROOT, check=True, env=env)


def rustflags():
    """Repo rustflags plus local-path remaps."""
    with open(os.path.join(ROOT, '.cargo', 'config.toml'), 'rb') as f:
        flags = tomllib.load(f).get('build', {}).get('rustflags', [])
    cargo_home = os.environ.get('CARGO_HOME') or os.path.join(os.path.expanduser('~'), '.cargo')
    flags += [f'--remap-path-prefix={cargo_home}=/cargo', f'--remap-path-prefix={ROOT}=/praetervnc']
    return '\x1f'.join(flags)


def check_paths(path):
    b = open(path, 'rb').read().lower()
    for p in {os.path.expanduser('~'), os.path.expanduser('~').replace('\\', '/')}:
        for enc in ('utf-8', 'utf-16-le'):
            if p.lower().encode(enc) in b:
                sys.exit(f'{path} contains the local path {p!r} ({enc})')


def main():
    ver = re.search(r'^version = "(.+)"', open(os.path.join(ROOT, 'Cargo.toml')).read(), re.M).group(1)
    run(sys.executable, os.path.join('tools', 'notices.py'))
    env = dict(os.environ, CARGO_TARGET_DIR=TARGET, CARGO_ENCODED_RUSTFLAGS=rustflags())
    run('cargo', 'build', '--release', '-p', 'praetervnc', env=env)
    exe = os.path.join(TARGET, 'release', 'praetervnc.exe')
    check_paths(exe)
    ico = max(glob.glob(os.path.join(TARGET, 'release', 'build', 'praetervnc-*', 'out', 'praetervnc.ico')), key=os.path.getmtime)
    msi = os.path.join(DIST, f'PraeterVNC-{ver}-x64.msi')
    portable = os.path.join(DIST, f'PraeterVNC-{ver}-portable.exe')
    extras = [os.path.join(DIST, n) for n in ('LICENSE.txt', 'THIRD-PARTY-NOTICES.txt', 'SHA256SUMS.txt')]
    # Artifacts only: a portable copy run from dist keeps its ini and logs.
    os.makedirs(DIST, exist_ok=True)
    for f in glob.glob(os.path.join(DIST, 'PraeterVNC-*')) + extras:
        try:
            if os.path.exists(f):
                os.remove(f)
        except PermissionError:
            sys.exit(f'{f} is in use; close the portable app first')
    run('dotnet', 'tool', 'restore')
    run('dotnet', 'wix', 'build', os.path.join('installer', 'PraeterVNC.wxs'), '-arch', 'x64',
        '-ext', 'WixToolset.Firewall.wixext', '-ext', 'WixToolset.Util.wixext',
        '-d', f'Version={ver}', '-d', f'Exe={exe}', '-d', f'Ico={ico}', '-d', f'Root={ROOT}', '-o', msi)
    for f in glob.glob(os.path.join(DIST, '*.wixpdb')):
        os.remove(f)
    check_paths(msi)
    shutil.copy(exe, portable)
    shutil.copy(os.path.join(ROOT, 'LICENSE'), os.path.join(DIST, 'LICENSE.txt'))
    shutil.copy(os.path.join(ROOT, 'THIRD-PARTY-NOTICES.txt'), DIST)
    with open(os.path.join(DIST, 'SHA256SUMS.txt'), 'w', newline='\n') as f:
        for p in (msi, portable):
            f.write(f'{hashlib.sha256(open(p, "rb").read()).hexdigest()}  {os.path.basename(p)}\n')
    for f in [msi, portable] + extras:
        print(f'{os.path.basename(f)}  {os.path.getsize(f) / 1e6:.2f} MB')


if __name__ == '__main__':
    main()
