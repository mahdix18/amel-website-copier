## Downloads

Each platform comes in two builds: the lean one, and **-browser**, which can also render JavaScript-built pages in Chromium (the browser is downloaded on first use with `amel browsers install`).

| Platform | Package |
|---|---|
| Windows (x86-64, ARM64) | `…-pc-windows-msvc.zip`: unzip and run `amel-gui.exe` |
| macOS (Apple Silicon: `aarch64`, Intel: `x86_64`) | `…-apple-darwin.dmg`: drag **Amel Website Copier** to Applications |
| Debian, Ubuntu (x86-64, ARM64) | `…-linux-gnu.deb`: `sudo apt install ./amel-….deb` |
| Other Linux | `…-linux-gnu.tar.gz`: unpack and run `./amel-gui` |

Every archive also has the `amel` command-line tool. `SHA256SUMS` lists the checksums.

The builds aren't signed yet. On macOS, if the app is reported as damaged or from an unidentified developer, run `xattr -dr com.apple.quarantine "/Applications/Amel Website Copier.app"` once. On Windows, SmartScreen may ask you to confirm: choose **More info → Run anyway**.
