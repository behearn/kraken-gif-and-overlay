# kraken-gif-and-overlay

Streams a GIF with a CPU and GPU temperature overlay to the **NZXT Kraken 2023 Elite** (USB `1e71:300c`) on Linux. 

This was written because liquidctl (which lists this cooler as `NZXT Kraken 2023 Elite (broken)`), and CoolerControl with the CoolerDash plugin, cannot (at time of writing) stream a GIF and apply an overlay on that model.

Ctrl+C switches the panel back to the liquid-temperature screen. If another program already has the cooler open, the streamer exits.

## Switches

```
--gif <path>           GIF to display or save
--position <0-100>     Square crop on a wide or tall GIF. 0 is left or top, 50 centers, 100 is right or bottom. Default: 50
--duration <seconds>   How long each image stays up when more than one is playing. Default: 120
--fade <seconds>       Fade in and fade out time between images. Default: 0.3
--order <mode>         sequential or random. Random never repeats the current image. Default: sequential
--save-config          Write config and exit. The first image is added. Later image changes need --add, --delete, or --update
--add                  Append --gif to the image list. Requires --save-config
--delete               Remove --gif from the image list. Requires --save-config
--update               Change the only configured image. Requires --save-config
--cpu-sensor <id>      CPU temperature sensor. Default: auto
--gpu-sensor <id>      GPU temperature sensor. Default: auto
--list-sensors         Print temperature sensors and exit
--reset                Restore the liquid temperature screen and exit
--box <yes|no>         Draw boxes behind the text on this image. Default: no
--opacity <0-255>      Box opacity for this image. Default: 150
--color <RRGGBB>       Text colour. Default: f2f2f2
--font <path>          .ttf font file
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

`--save-config` copies the GIF into `~/.local/share/kraken-gif-and-overlay/`, writes `config.yml`, and exits. The first save adds the image. Once an image is in the list, saving another image change needs `--add`, `--delete`, or, when only one image is configured, `--update`. Slideshow, sensor, and text settings can be saved on their own. The service reads that config and does not take switches.

```bash
/usr/local/bin/kraken-gif-and-overlay --gif <path> --save-config
/usr/local/bin/kraken-gif-and-overlay --gif <path> --position 0 --box yes --opacity 180 --save-config --add
/usr/local/bin/kraken-gif-and-overlay --gif <path> --save-config --delete
/usr/local/bin/kraken-gif-and-overlay --position 100 --save-config --update
/usr/local/bin/kraken-gif-and-overlay --duration 15 --fade 1 --order random --save-config
```

When more than one image is configured, they rotate. `--duration` is how many seconds each image stays up. `--fade` is how long the GIF takes to fade in from black and out to black. The temperatures stay at full strength. `--order sequential` follows the list. `--order random` picks any image except the one on screen. A single image loops, and duration and fade are ignored.

A GIF that is not square is cropped to its shorter side, then scaled, so a circle stays a circle. `--position` slides that square along the longer side: `0` keeps the left or top, `100` keeps the right or bottom, and `50` (the default) centers it. Position, box, and box opacity are stored on each image.

`color` is `RRGGBB` or `r,g,b`. The default colour is off-white `f2f2f2`. `font` is a `.ttf` file. When it is unset, DejaVu Sans Bold is used, then Noto Sans Bold. Colour and font apply to every image.

```bash
/usr/local/bin/kraken-gif-and-overlay --color f2f2f2 --font /usr/share/fonts/TTF/DejaVuSans-Bold.ttf --save-config
```

Example `config.yml`:

```yaml
sensors:
  cpu: k10temp:Tctl
  gpu: amdgpu:edge@0000:03:00.0
overlay:
  font: /usr/share/fonts/TTF/DejaVuSans-Bold.ttf
  color: f2f2f2
slideshow:
  duration: 120
  fade: 0.3
  order: sequential
images:
  - gifPath: cat-jam.gif
    box: false
    boxOpacity: 150
    position: 50
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

`./install.sh` asks for sudo. It installs the binary to `/usr/local/bin/kraken-gif-and-overlay`, enables a systemd service for the user who ran it, and adds a udev rule so that user can open the Kraken at boot. The service reads that user's `~/.local/share/kraken-gif-and-overlay/config.yml` and does not load shell startup files. Save a config before the service will start.

```bash
./build.sh
./install.sh
```

`sudo systemctl stop kraken-gif-and-overlay` sends SIGTERM. The program catches that, the same way it catches Ctrl+C, and switches the panel back to the liquid temperature screen.

If the process was killed and the GIF is still on the panel, stop the service and then force the liquid screen:

```bash
sudo systemctl stop kraken-gif-and-overlay
/usr/local/bin/kraken-gif-and-overlay --reset
```

`sudo systemctl kill --signal=SIGINT kraken-gif-and-overlay` is the other way to ask the running process to exit. It sends the Ctrl+C signal and does not follow it with SIGKILL.

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

## Known issues

Dropped frames, or the panel flashing back to the liquid temperature for a moment, usually means another program still has the cooler. CoolerControl and CoolerDash do this when they are still installed: stopping this service hands the screen back to them, and while both are running the GIF stutters. `coolercontrold` restarts itself, so stopping the process is not enough.

```bash
systemctl status coolercontrold coolerdash cc-plugin-coolerdash coolercontrol-lcd-recover.timer
sudo systemctl disable --now coolercontrold.service coolerdash.service cc-plugin-coolerdash.service coolercontrol-lcd-recover.timer
```

## License

The program is MIT. See [LICENSE](LICENSE).
