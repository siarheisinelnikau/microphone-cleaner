"""Collect mandatory dependency license text from Cargo metadata, without network access."""
import json
import hashlib
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]

def main():
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--offline", "--locked", "--filter-platform", "x86_64-pc-windows-msvc", "--format-version", "1"], cwd=ROOT
    ))
    target = ROOT / "THIRD_PARTY_NOTICES.txt"
    prefix = target.read_text(encoding="utf-8").split("\n===== Dependency licenses =====", 1)[0].rstrip()
    sections = [prefix, "\n===== Dependency licenses =====\n"]
    missing = []
    texts = {}
    graph = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    used = set()
    pending = [metadata["resolve"]["root"]]
    while pending:
        package_id = pending.pop()
        if package_id in used:
            continue
        used.add(package_id)
        pending.extend(dependency["pkg"] for dependency in graph[package_id]["deps"])
    count = 0
    for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
        if package["id"] not in used:
            continue
        if package["name"] == "microphone-cleaner":
            continue
        directory = Path(package["manifest_path"]).parent
        supplemental = ROOT / "third-party" / package["name"]
        if supplemental.is_dir():
            directory = supplemental
        files = sorted(p for p in directory.iterdir() if p.is_file() and
            (p.name.lower().startswith(("license", "copying", "copyright"))))
        explicit = package.get("license_file")
        if explicit and (directory / explicit).is_file() and (directory / explicit) not in files:
            files.append(directory / explicit)
        if not files:
            missing.append(package["name"])
        sections.append(f"\n--- {package['name']} {package['version']} | {package.get('license')} ---\n"
                        f"{package.get('repository') or ''}\n")
        for path in files:
            text = path.read_text(encoding='utf-8', errors='strict').rstrip()
            identifier = hashlib.sha256(text.encode('utf-8')).hexdigest()[:16]
            texts.setdefault(identifier, text)
            sections.append(f"[{path.name}] License text {identifier}\n")
        count += 1
    if missing:
        raise SystemExit("Missing license files: " + ", ".join(missing))
    for identifier, text in texts.items():
        sections.append(f"\n===== License text {identifier} =====\n{text}\n")
    target.write_text("\n".join(sections), encoding="utf-8", newline="\n")
    print(f"Collected Windows dependency license text for {count} packages.")

if __name__ == "__main__":
    main()
