# kraken-gif-and-overlay

Streams a GIF with a CPU and GPU temperature overlay to the **NZXT Kraken 2023 Elite** (USB `1e71:300c`) on Linux. 

This was written because liquidctl (which lists this cooler as `NZXT Kraken 2023 Elite (broken)`), and CoolerControl with the CoolerDash plugin, cannot (at time of writing) stream a GIF and apply an overlay on that model.

Ctrl+C switches the panel back to the liquid-temperature screen. If another program already has the cooler open, the streamer exits.

## Switches

```
--gif <path>           GIF to display
--position <0-100>     Square crop on a wide or tall GIF. 0 is left or top, 50 centers, 100 is right or bottom. Default: 50
--cpu-sensor <id>      CPU temperature sensor. Default: auto
--gpu-sensor <id>      GPU temperature sensor. Default: auto
--list-sensors         Print temperature sensors and exit
--box <yes|no>         Draw boxes behind the text. Default: no
--opacity <0-255>      Box opacity. Default: 150
--color <RRGGBB>       Text colour. Default: f2f2f2
--font <path>          .ttf font file
--save-config          Copy --gif into the data directory and write config
--debug                Print frame stats about every two seconds
--help                 Show this help
```

With no switches and no config file, the program prints this list and exits. Config is written only when `--save-config` is passed.

## Quick start

`./install.sh` installs the program to `/usr/local/bin/kraken-gif-and-overlay`. 

Show a GIF without writing a config:
```bash
/usr/local/bin/kraken-gif-and-overlay --gif <path>
```

## Setup

CPU and GPU temperatures are detected automatically. The CPU sensor is `Tctl`, `Tdie`, or `Package id 0` on coretemp, k10temp, zenpower, or cpu_thermal. The GPU sensor is a graphics chip that exposes a fan (amdgpu, nvidia, nouveau, i915, or xe). Override either one with `--cpu-sensor` or `--gpu-sensor`.

`--list-sensors` prints every temperature sensor, then the cpu and gpu sensors that would be used:

```bash
/usr/local/bin/kraken-gif-and-overlay --list-sensors
```

`--save-config` copies the GIF into `~/.local/share/kraken-gif-and-overlay/` and writes `config` there. Keys already in the file are kept. The service reads that config and does not take switches.

```bash
/usr/local/bin/kraken-gif-and-overlay --gif <path> --save-config
/usr/local/bin/kraken-gif-and-overlay --cpu-sensor <id> --gpu-sensor <id> --save-config
```

A GIF that is not square is cropped to its shorter side, then scaled, so a circle stays a circle. `--position` slides that square along the longer side: `0` keeps the left or top, `100` keeps the right or bottom, and `50` (the default) centers it.

Pass `--position` or the overlay switches with `--save-config` to store them too. `box` is `yes` or `no`. `opacity` is 0–255 and applies to the dark boxes behind the text. `color` is `RRGGBB` or `r,g,b`. The default colour is off-white `f2f2f2`. `font` is a `.ttf` file. When it is unset, DejaVu Sans Bold is used, then Noto Sans Bold.

```bash
/usr/local/bin/kraken-gif-and-overlay --box yes --opacity 150 --color f2f2f2 --font /usr/share/fonts/TTF/DejaVuSans-Bold.ttf --save-config
```

Example `config`:

```
gif = cat-jam.gif
cpu = k10temp:Tctl
gpu = amdgpu:edge@0000:03:00.0
```

## Build

Requires Rust, libusb, and hidapi.

```bash
./build.sh
```

`cargo run` builds the unoptimized debug binary, which spends a long time scaling the GIF before it opens the cooler. Use the release profile instead:

```bash
cargo run --release -- --gif <path>
```

## Install as a system service

`./install.sh` asks for sudo. It installs the binary to `/usr/local/bin/kraken-gif-and-overlay`, enables a systemd service for the user who ran it, and adds a udev rule so that user can open the Kraken at boot. The service reads that user's `~/.local/share/kraken-gif-and-overlay/config` and does not load shell startup files. Save a config before the service will start.

```bash
./build.sh
./install.sh
```

`sudo systemctl stop kraken-gif-and-overlay` restores the liquid temperature screen.

Stdout and stderr go to the journal. Recent lines, and a follow that stays open:

```bash
journalctl -u kraken-gif-and-overlay -e
journalctl -u kraken-gif-and-overlay -f
```

If the cooler is already open, the journal shows that the Kraken is in use. CoolerControl uses the same USB device. After a run of failed starts, clear the limit and start again:

```bash
sudo systemctl reset-failed kraken-gif-and-overlay
sudo systemctl start kraken-gif-and-overlay
```

## License

The program is MIT. See [LICENSE](LICENSE).
