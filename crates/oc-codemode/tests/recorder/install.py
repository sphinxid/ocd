"""Installs a recording into crates/oc-codemode/tests/fixtures/upstream, keeping a deterministic quarter of the
test262 shards (records whose sha1(test) is divisible by 4), like the previous checked-in set."""
import hashlib, json, os, shutil, sys

src, dst = sys.argv[1], sys.argv[2]
for name in os.listdir(dst):
    if name == "README.md":
        continue
    path = os.path.join(dst, name)
    shutil.rmtree(path) if os.path.isdir(path) else os.remove(path)
for name in os.listdir(src):
    path = os.path.join(src, name)
    if name == "test262" or name == "README.md":
        continue
    shutil.copy(path, os.path.join(dst, name))
kept = total = 0
for root, _, files in os.walk(os.path.join(src, "test262")):
    for f in sorted(files):
        rel = os.path.relpath(os.path.join(root, f), src)
        out = []
        for line in open(os.path.join(root, f), encoding="utf-8"):
            if not line.strip():
                continue
            total += 1
            test = json.loads(line)["test"]
            if int(hashlib.sha1(test.encode()).hexdigest(), 16) % 4 == 0:
                out.append(line if line.endswith("\n") else line + "\n")
        if out:
            os.makedirs(os.path.dirname(os.path.join(dst, rel)), exist_ok=True)
            with open(os.path.join(dst, rel), "w", encoding="utf-8") as h:
                h.writelines(out)
            kept += len(out)
print(f"test262: kept {kept} of {total}")
