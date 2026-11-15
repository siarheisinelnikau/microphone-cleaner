"""Inspect packaged PE imports, manifest and actual icon resources without loading an EXE."""
import json
import struct
import sys
from pathlib import Path

def audit(path):
    data = Path(path).read_bytes()
    assert data[:2] == b'MZ', 'Not a Windows executable'
    pe = struct.unpack_from('<I', data, 60)[0]
    assert data[pe:pe + 4] == b'PE\0\0'
    machine, count = struct.unpack_from('<HH', data, pe + 4)
    optional_length = struct.unpack_from('<H', data, pe + 20)[0]
    optional = pe + 24
    magic = struct.unpack_from('<H', data, optional)[0]
    assert machine == 0x8664 and magic == 0x20B, 'The unsuffixed distribution must be Windows x64'
    subsystem = struct.unpack_from('<H', data, optional + 68)[0]
    assert subsystem == 2, 'Expected the Windows GUI subsystem'
    directories = optional + 112
    sections = []
    for index in range(count):
        start = optional + optional_length + index * 40
        virtual_size, address, raw_size, raw = struct.unpack_from('<IIII', data, start + 8)
        sections.append((address, max(virtual_size, raw_size), raw))
    def at(rva):
        for address, length, raw in sections:
            if address <= rva < address + length:
                return raw + rva - address
        raise AssertionError(f'RVA {rva:x} is outside the image')
    resource_rva, _ = struct.unpack_from('<II', data, directories + 2 * 8)
    root = at(resource_rva)
    resources = {}
    def walk(offset, ids=()):
        named, numbered = struct.unpack_from('<HH', data, root + offset + 12)
        for index in range(named + numbered):
            name, target = struct.unpack_from('<II', data, root + offset + 16 + index * 8)
            if target & 0x80000000:
                walk(target & 0x7fffffff, ids + (name,))
            else:
                rva, length = struct.unpack_from('<II', data, root + target)
                resources[ids + (name,)] = data[at(rva):at(rva) + length]
    walk(0)
    groups = [value for ids, value in resources.items() if ids[0] == 14]
    icons = {ids[1]: value for ids, value in resources.items() if ids[0] == 3}
    assert len(groups) == 1 and icons, 'Application icon resources are missing'
    group = groups[0]
    _, kind, variants = struct.unpack_from('<HHH', group)
    assert kind == 1 and variants >= 8
    icon_sizes = []
    for index in range(variants):
        start = 6 + index * 14
        width, height = struct.unpack_from('<BB', group, start)
        identifier = struct.unpack_from('<H', group, start + 12)[0]
        dib = icons[identifier]
        header, pixels, doubled_height = struct.unpack_from('<III', dib)
        depth = struct.unpack_from('<H', dib, 14)[0]
        assert header == 40 and depth == 32 and pixels == (width or 256) and doubled_height == 2 * (height or 256)
        rgba = dib[40:40 + pixels * pixels * 4]
        opaque = sum(rgba[index] != 0 for index in range(3, len(rgba), 4))
        assert opaque > pixels * pixels // 2, 'Blank or excessively padded icon'
        assert any(rgba[index] == 0 for index in range(3, len(rgba), 4)), 'Expected transparent icon corners'
        icon_sizes.append(pixels)
    assert {16, 20, 24, 32, 48, 64, 128, 256} <= set(icon_sizes)
    manifests = [value for ids, value in resources.items() if ids[0] == 24]
    assert manifests and b'asInvoker' in manifests[0] and b'PerMonitorV2' in manifests[0], 'Manifest or non-elevated DPI configuration is missing'
    assert b'ProductName' in data or 'ProductName'.encode('utf-16le') in data
    import_rva, _ = struct.unpack_from('<II', data, directories + 8)
    imports = []
    if import_rva:
        offset = at(import_rva)
        while any(data[offset:offset + 20]):
            name_rva = struct.unpack_from('<I', data, offset + 12)[0]
            begin = at(name_rva)
            imports.append(data[begin:data.index(0, begin)].decode('ascii'))
            offset += 20
    assert not any(name.lower().startswith(('opengl', 'd3d', 'dxgi', 'vulkan')) for name in imports), 'Unexpected application graphics backend'
    assert not any(name.lower().startswith(('vcruntime', 'msvcp')) for name in imports), 'The portable build must statically link the VC runtime'
    return {'architecture': 'Windows x64', 'subsystem': 'GUI', 'icon_sizes': sorted(icon_sizes),
            'manifest': 'asInvoker / PerMonitorV2', 'imports': sorted(imports), 'bytes': len(data)}

if __name__ == '__main__':
    print(json.dumps(audit(sys.argv[1]), ensure_ascii=False))
