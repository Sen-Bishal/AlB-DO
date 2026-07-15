import os, re, subprocess
from PIL import Image

ROOT   = r"A:\AlBDO-v-0.1.0\brief"
INSPO  = r"C:\Users\bisha\Downloads\inspo"
ASSETS = os.path.join(ROOT, "assets")
SRC    = os.path.join(ROOT, "brief.html")
OUT    = r"A:\AlBDO-v-0.1.0\ALKMY_Brand_Identity_Brief.pdf"

os.makedirs(ASSETS, exist_ok=True)

# the one messy source filename maps to a clean asset name used in the HTML
RENAME = {
    "by edición limitada_@noakabes____3 _3 _3 prototipo nkbs gaseosa__"
    "#graphic #design #branding #brand #typography #3d #art #bdsgn.jpg": "nkbs_can.jpg",
}

def downscale(src_path, dst_path, max_w=760, q=80):
    im = Image.open(src_path).convert("RGB")
    if im.width > max_w:
        im = im.resize((max_w, round(im.height * max_w / im.width)), Image.LANCZOS)
    im.save(dst_path, "JPEG", quality=q, optimize=True)

# populate assets/ for every source image
for fn in os.listdir(INSPO):
    if not fn.lower().endswith((".jpg", ".jpeg", ".png")):
        continue
    dst = RENAME.get(fn, fn)
    downscale(os.path.join(INSPO, fn), os.path.join(ASSETS, dst))

# sanity: every asset referenced by the HTML must exist
refs = set(re.findall(r'src="assets/([^"]+)"', open(SRC, encoding="utf-8").read()))
missing = [r for r in refs if not os.path.exists(os.path.join(ASSETS, r))]
print("referenced:", len(refs), "| missing:", missing or "none")

# render brief.html (relative assets/ resolve from file:// URL) to PDF
chrome = r"C:\Program Files\Google\Chrome\Application\chrome.exe"
url = "file:///" + SRC.replace("\\", "/")
cmd = [chrome, "--headless=new", "--disable-gpu", "--no-pdf-header-footer",
       "--virtual-time-budget=20000", f"--print-to-pdf={OUT}", url]
r = subprocess.run(cmd, capture_output=True, text=True)
print("chrome rc:", r.returncode,
      "| pdf:", f"{os.path.getsize(OUT)/1024:.0f} KB" if os.path.exists(OUT) else "MISSING")
