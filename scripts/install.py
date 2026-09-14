#!/usr/bin/env python3
"""Install an extracted NeuralForge AppDir; remove only unchanged tracked files.
No system package, upstream path, config, runtime file or Wine prefix is removed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil

APP_ID = 'io.github.labj1987.NeuralForge'
LAYER = 'VK_LAYER_neuralforge_neural'

def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['install', 'uninstall', 'archive-legacy-manifest'])
    parser.add_argument('--appdir', type=Path)
    parser.add_argument('--legacy-manifest', type=Path)
    args = parser.parse_args()
    data = Path(os.environ.get('XDG_DATA_HOME', str(Path.home() / '.local/share')))
    root = data / 'neuralforge'
    record = root / 'installation.json'
    if args.command == 'archive-legacy-manifest':
        # This exact identity belongs to this Rust repository; upstream's NV
        # layer identity is different. Never infer ownership from directory names.
        src = args.legacy_manifest
        if src is None or src.is_symlink():
            parser.error('provide a regular --legacy-manifest file')
        layer = json.loads(src.read_text()).get('layer', {})
        if (layer.get('name') != 'VK_LAYER_dlssnr_neural'
                or Path(layer.get('library_path', '')).name != 'libdlssnr_layer.so'
                or layer.get('enable_environment') != {'VKLayer_DLSS5': '1'}
                or layer.get('disable_environment') != {'DLSSNR_DISABLE': '1'}):
            parser.error('manifest does not match this repository’s legacy identity')
        archive = root / 'legacy' / 'VK_LAYER_dlssnr_neural.json.disabled'
        archive.parent.mkdir(parents=True, exist_ok=True)
        with archive.open('xb') as output:
            output.write(src.read_bytes())
        src.unlink()
        print(f'Archived {src} to {archive}; config, runtime and libraries untouched')
        return
    old = json.loads(record.read_text()) if record.exists() else {}
    if args.command == 'uninstall':
        for name, expected in old.items():
            path = Path(name)
            if path.is_file() and not path.is_symlink() and digest(path) == expected:
                path.unlink()
            else:
                print(f'Preserved changed/missing file: {path}')
        if record.exists(): record.unlink()
        return
    if args.appdir is None: parser.error('install requires --appdir')
    usr = args.appdir / 'usr'
    files = {}
    for src in (usr / 'lib/neuralforge').rglob('*'):
        if src.is_file(): files[root / 'lib/neuralforge' / src.relative_to(usr / 'lib/neuralforge')] = src.read_bytes()
    for binary in ['neuralforge', 'neuralforge-cli']:
        files[root / 'bin' / binary] = (usr / 'bin' / binary).read_bytes()
    manifest = json.loads((usr / f'share/vulkan/implicit_layer.d/{LAYER}.json').read_text())
    assert manifest['layer']['name'] == LAYER
    manifest['layer']['library_path'] = str(root / 'lib/neuralforge/libneuralforge_layer.so')
    files[data / f'vulkan/implicit_layer.d/{LAYER}.json'] = json.dumps(manifest, indent=2).encode()
    desktop = (usr / f'share/applications/{APP_ID}.desktop').read_text()
    desktop = desktop.replace('Exec=neuralforge', f'Exec="{root / "bin/neuralforge"}"')
    files[data / f'applications/{APP_ID}.desktop'] = desktop.encode()
    for sub, name in [('icons/hicolor/scalable/apps', 'neuralforge.svg'), ('metainfo', f'{APP_ID}.appdata.xml')]:
        files[data / sub / name] = (usr / 'share' / sub / name).read_bytes()
    # Validate all destinations before writing any file.
    for path in files:
        if any(parent.is_symlink() for parent in [path, *path.parents]):
            parser.error(f'refusing symlink destination: {path}')
        if path.exists() and old.get(str(path)) != digest(path):
            parser.error(f'refusing to overwrite unowned or changed file: {path}')
    new = {}
    for path, content in files.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        path.chmod(0o755 if path.parent == root / 'bin' else 0o644)
        new[str(path)] = digest(path)
    record.write_text(json.dumps(new, indent=2) + '\n')
    print(f'Installed NeuralForge. CLI: {root / "bin/neuralforge-cli"}')

if __name__ == '__main__': main()
