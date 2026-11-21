#!/usr/bin/env python3
"""Makes the app's other icons from assets/icon.png (needs Pillow):

  assets/icon.ico   the Windows .exe icon, 16 to 256 px
  assets/logo.png   the 96 px logo in the window's title bar
  assets/watermark.png  the 512 px cat in the window's corner

The macOS .icns is made from icon.png by scripts/package.sh.

    python3 scripts/icons.py
"""
from pathlib import Path

from PIL import Image

assets = Path(__file__).resolve().parent.parent / "assets"
icon = Image.open(assets / "icon.png").convert("RGBA")
if icon.width != icon.height:
    raise SystemExit("assets/icon.png must be square")
icon.save(assets / "icon.ico", sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])
icon.resize((96, 96), Image.LANCZOS).save(assets / "logo.png")
icon.resize((512, 512), Image.LANCZOS).save(assets / "watermark.png", optimize=True)
print("wrote assets/icon.ico, assets/logo.png and assets/watermark.png")
