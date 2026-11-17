# monswitch – česky

Přepínání monitoru mezi počítači z klávesnice, z menu nebo z příkazové řádky (Windows a macOS).
Podrobná dokumentace je v [anglickém README](README.md); tohle je rychlý návod.

## Co umí

- **Přepnout vstup monitoru** přes DDC/CI – globální zkratkou, z tray/menubar aplikace nebo
  příkazem `monswitch input hdmi1`.
- **PiP / PbP** (obraz v obraze, dva počítače vedle sebe) – u monitorů s presetem.
- **Software KVM** v režimu PbP: jedna klávesnice a myš přes obě poloviny + sdílená schránka
  (Deskflow, monswitch ho sám spouští a zastavuje).
- **Efekt při přechodu** kurzoru na druhý počítač (mlha + světlo na hraně, záře, částice).
- **Streaming** druhého počítače do okna nebo hraní her (Sunshine + Moonlight).

## Pojede to na mém monitoru?

**Přepínání vstupů skoro jistě ano** – podporuje ho téměř každý monitor s DDC/CI (zapni ho v OSD).
Standardní hodnoty (DisplayPort, HDMI) jsou v configu; USB-C/Thunderbolt nemá standardní hodnotu –
`monswitch caps` ukáže, co monitor přijímá. Doky, huby a DisplayLink DDC často nepropustí.

**PiP / PbP potřebuje preset**, protože každý výrobce má vlastní kódy. Hotový je pro
ASUS ROG Swift PG27UCDM (`monswitch config preset asus-pg27ucdm`); pro jiné monitory je v README
návod, jak kódy najít příkazem `monswitch probe`.

## Rychlý start

1. Stáhni instalátor z [Releases](../../releases) (macOS: poprvé pravé tlačítko → *Otevřít*),
   nebo sestav ze zdrojáků podle README.
2. V terminálu:
   ```sh
   monswitch list          # najde monitor
   monswitch caps          # ukáže podporované vstupy
   monswitch input hdmi1   # přepne
   ```
3. Spusť tray aplikaci → **Settings…** → nastav vstupy, počítače a zkratky
   (výchozí <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>Shift</kbd>+<kbd>1</kbd>/<kbd>2</kbd>) → **Save**.
4. Totéž na druhém počítači.

KVM a streaming: postup krok za krokem je v README v sekcích
[Software KVM](README.md#software-kvm-in-pbp-mode) a [Streaming](README.md#streaming).

Config: Windows `%APPDATA%\monswitch\config.toml`, macOS `~/.config/monswitch/config.toml`.
