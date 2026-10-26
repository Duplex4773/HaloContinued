"""Import reviewed 1.15 battery identities, without executing upstream Python.

Usage: python tools/sync-upstream-catalog.py --upstream PATH
Only the tables whose protocols have been ported are read. Control write
allowlists are deliberately separate and never changed by this tool.
"""
import argparse
import ast
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--upstream', type=Path, required=True)
args = parser.parse_args()
symbols = {}

def value(node):
    if isinstance(node, ast.Name):
        return symbols.get(node.id, node.id)
    if isinstance(node, (ast.Tuple, ast.List)):
        return [value(n) for n in node.elts]
    if isinstance(node, ast.Dict):
        return {value(k): value(v) for k, v in zip(node.keys, node.values)}
    return ast.literal_eval(node)

def table(module, name):
    tree = ast.parse((args.upstream / 'providers' / (module + '.py')).read_text(encoding='utf-8'))
    symbols.clear()
    for node in tree.body:
        if isinstance(node, ast.Assign) and isinstance(node.value, ast.Constant):
            for target in node.targets:
                if isinstance(target, ast.Name):
                    symbols[target.id] = node.value.value
    for node in tree.body:
        targets = node.targets if isinstance(node, ast.Assign) else [node.target] if isinstance(node, ast.AnnAssign) else []
        if any(isinstance(t, ast.Name) and t.id == name for t in targets):
            return value(node.value)
    raise ValueError(f'Missing {module}.{name}')

rows = {tuple(r[:3]): r for r in json.loads((ROOT / 'crates/providers/tests/catalog-fixtures.json').read_text())}
def add(provider, vid, pid, name, variant='', parameter=0):
    rows[(provider, vid, pid)] = [provider, vid, pid, name, variant, parameter]

for pid, entry in table('razer', 'KNOWN').items():
    add('razer', 0x1532, pid, entry[0], parameter=entry[1])
for pid, name in table('hyperx_cloud3', 'PIDS').items():
    add('hyperx_cloud3', 0x03f0, pid, name)
for vid, devices in table('pulsar', 'PIDS').items():
    for pid, name in devices.items():
        add('pulsar', vid, pid, name)
for pid, entry in table('steelseries', 'MODELS').items():
    add('steelseries', 0x1038, pid, entry[0], entry[-1])
for pid, name in table('corsair', 'BRAGI_PIDS').items():
    add('corsair', 0x1b1c, pid, name, 'bragi')
for pid, name in table('mchose', 'A5_PIDS').items():
    add('mchose', 0x2023, pid, name)
for pid in table('nzxt', 'PIDS'):
    add('nzxt', 0x1e71, pid, 'NZXT Lift Elite Wireless')
for pid in table('steam', 'PUCKS'):
    add('steam', 0x28de, pid, 'Steam Controller (2025)')
# Hardware-confirmed additions from reviewed, pinned parent pull requests. Apply
# last so importing the older main catalog cannot remove their protocol routing.
for entry in json.loads((ROOT / 'crates/providers/reviewed-devices.json').read_text()):
    add(*entry['device'])
ordered = sorted(rows.values())
catalog = ROOT / 'crates/providers/src/catalog.rs'
old = catalog.read_text(encoding='utf-8')
start, end = old.index('pub const DEVICES:'), old.index('pub const AUDEZE_REQUESTS:')
lines = ['pub const DEVICES: &[Device] = &[']
for provider, vid, pid, name, variant, parameter in ordered:
    lines.append(f'Device {{ provider:{json.dumps(provider)}, vid:0x{vid:04x}, pid:0x{pid:04x}, name:{json.dumps(name)}, variant:{json.dumps(variant)}, parameter:{parameter} }},')
lines.append('];\n')
catalog.write_text(old[:start].replace('upstream 1.14.0', 'upstream 1.15.0').replace('tools/port_catalog.py', 'tools/sync-upstream-catalog.py') + '\n'.join(lines) + old[end:], encoding='utf-8')
(ROOT / 'crates/providers/tests/catalog-fixtures.json').write_text(json.dumps(ordered, indent=2) + '\n', encoding='utf-8')
print(f'{len(ordered)} reviewed battery identities; device-control allowlists untouched.')

# Keep the display-only parent verification list aligned with the reviewed IDs.
# This metadata never expands a device-control allowlist.
verified = {
    'razer:0528:', 'steam:1304:', 'steam:1305:', 'nzxt:2101:', 'nzxt:2129:',
    'mchose:f013:', 'hyperx_cloud3:0995:', 'hyperx_cloud3:0795:',
    'pulsar:5406:', 'pulsar:3414:', 'steelseries:2317:', 'corsair:1b7f:', 'corsair:0a40:',
}
for entry in json.loads((ROOT / 'crates/providers/reviewed-devices.json').read_text()):
    provider, _, pid, _, variant, _ = entry['device']
    protocol = variant.removeprefix('pr:')
    family = {'nearlink':0x108c, 'ajazz':0x5c2f, 'aula-mouse':0x22a0, 'turbo':0x6008}.get(protocol, pid)
    if protocol == 'rapoo':
        family = 0x1413 if pid in (0x1413,0x4613) else 0x1924
    if protocol == 'turtle':
        family = 0x229b if pid in (0x229b,0x229e) else 0x2251
    if provider not in ('shield','xinput'):
        verified.add(f'{provider}:{family:04x}:')
metadata = ROOT / 'crates/providers/src/device_metadata.rs'
text = metadata.read_text(encoding='utf-8')
start = text.index('const PARENT_PREFIXES:')
end = text.index('];', start) + 2
text = text[:start] + 'const PARENT_PREFIXES: &[&str] = &[\n' + ''.join(f'    "{p}",\n' for p in sorted(verified)) + '];' + text[end:]
metadata.write_text(text, encoding='utf-8')
