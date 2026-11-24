"""Refresh the offline catalog using read-only exact lookups on an English WinGet CLI."""
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
import json
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
KEYS = dict(version="Version", publisher="Publisher", description="Description",
            license="License", website="Homepage", notes="Release Notes Url")
MAX_LENGTH = dict(version=120, publisher=120, description=720, license=120, website=512, notes=512)

def resolve(row):
    package, category, shelf, summary = row
    result = subprocess.run(
        ["winget", "show", "--id", package, "--exact", "--source", "winget",
         "--disable-interactivity"], capture_output=True, encoding="utf-8", errors="replace",
        creationflags=subprocess.CREATE_NO_WINDOW, timeout=35,
    )
    text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", result.stdout)
    found = re.search(r"^Found (.+) \[" + re.escape(package) + r"\]$", text, re.M)
    if not found or result.returncode:
        return None, package
    record = dict(id=package, name=re.sub(r" \([a-z]{2}-[A-Z]{2}\)$", "", found[1]), category=category, shelf=shelf, summary=summary)
    for key, heading in KEYS.items():
        if key == "description":
            match = re.search(r"^Description:[ \t]*(.*)(\n(?:[ \t]+[^\n]*\n?)*)?", text, re.M)
            value = " ".join(match[0].removeprefix("Description:").split()) if match else ""
            if len(value) > MAX_LENGTH[key]:
                value = value[:MAX_LENGTH[key] - 1].rsplit(" ", 1)[0] + "\u2026"
        else:
            match = re.search(r"^"+heading+r":[ \t]+(.+)$", text, re.M)
            value = match[1].strip() if match else ""
        if key in ("website", "notes") and not re.fullmatch(r"https://[^\s]+", value):
            value = ""
        record[key] = value if len(value) <= MAX_LENGTH[key] else ""
    return record, None

if __name__ == "__main__":
    lines = (ROOT / "catalog/editorial.txt").read_text(encoding="utf-8-sig").splitlines()
    rows = [line.strip().split("|") for line in lines if line and not line.startswith("#")]
    entries, unresolved = [], []
    with ThreadPoolExecutor(4) as executor:
        for entry, missing in executor.map(resolve, rows):
            if entry:
                entries.append(entry)
            else:
                unresolved.append(missing)
    print(f"Exact WinGet resolutions: {len(entries)}/{len(rows)}")
    print("Unresolved: " + (", ".join(unresolved) or "none"))
    if unresolved:
        raise SystemExit(1)
    (ROOT / "catalog/verified.json").write_text(
        json.dumps(entries, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
