//! Stream a GIF to a Kraken 2023 Elite using the Q565 path CAM uses.
//!
//! Each frame is interrupt `36 01 00 01 08`, a 20-byte header with mode 0x08,
//! the Q565 bytes on bulk endpoint 0x02, then interrupt `36 02`.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ab_glyph::{FontArc, PxScale, ScaleFont};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use hidapi::HidDevice;
use image::imageops::FilterType;
use image::{AnimationDecoder, Rgb, RgbImage};
use imageproc::drawing::draw_text_mut;
use q565::encode::Q565EncodeContext;
use q565::utils::{encode_rgb565_unchecked, rgb888_to_rgb565};
use rusb::{DeviceHandle, Direction, TransferType, UsbContext};

mod protocol;

use protocol::{
    bulk_header, orientation_degrees, orientation_report, ELITE_PID, FRAME_COMMIT, FRAME_COMMIT_ACK,
    FRAME_SETUP, FRAME_SETUP_ACK, HEIGHT, LCD_BRIGHTNESS_OFFSET, LCD_INFO_PREFIX, LCD_ORIENTATION_OFFSET,
    LCD_QUERY, LIQUID_SCREEN, NZXT_VID, WIDTH,
};

fn main() -> Result<()> {
    let args = parse_args()?;
    match rotation_request(&args)? {
        Some(RotationCmd::Show) => return show_rotation(),
        Some(RotationCmd::Set(degrees)) => return set_rotation(degrees),
        None => {}
    }
    if args.help || (!args.any && !config_exists()?) {
        print_usage();
        if args.help {
            return Ok(());
        }
        std::process::exit(1);
    }

    if (args.add || args.delete || args.update) && !args.save_config {
        bail!("Pass --save-config with --add, --delete, or --update.");
    }

    if args.reset {
        return reset_liquid();
    }

    let running = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&running);
    ctrlc::set_handler(move || flag.store(false, Ordering::Relaxed))?;

    if args.list_sensors {
        print_sensors(&scan_sensors());
        return Ok(());
    }

    let Some(settings) = load_settings(&args)? else {
        return Ok(());
    };
    let mut player = Player::new(settings.images.len(), settings.hold, settings.fade, settings.order);
    let mut library = Library::open(settings.images, player.index, args.debug)?;
    let font = load_font(&settings.font)?;
    let frame_count = library.current.frames.len();
    if library.images.len() == 1 {
        println!("Kraken 2023 Elite: {WIDTH}x{HEIGHT}, {frame_count} GIF frames, Q565 stream");
    } else {
        println!(
            "Kraken 2023 Elite: {WIDTH}x{HEIGHT}, {} images, Q565 stream",
            library.images.len()
        );
        println!(
            "Slideshow: {}s on each image, fade {}s, {}",
            settings.hold.as_secs_f64(),
            settings.fade.as_secs_f64(),
            settings.order.as_str()
        );
    }

    let usb = rusb::Context::new().context("USB context")?;
    let mut bulk = open_kraken(&usb)?;
    let endpoint = claim_bulk_out(&mut bulk)?;

    let hid_api = hidapi::HidApi::new()?;
    let hid = hid_api
        .open(NZXT_VID, ELITE_PID)
        .map_err(hid_busy)
        .context("HID interface")?;

    let _restore = LiquidRestore { hid: &hid };
    let (_, orientation) = lcd_info(&hid)?;
    let rotation = orientation_degrees(orientation)?;
    println!("Frame rotation {rotation}°");
    if let Some(next) = player.planned_next() {
        library.prefetch(next);
    }
    let mut sensors = SensorCache {
        cpu: settings.cpu,
        gpu: settings.gpu,
        cpu_c: None,
        gpu_c: None,
        read_at: None,
    };
    let mut sent = 0u32;
    let mut window = Instant::now();
    let mut deadline = Instant::now();

    while running.load(Ordering::Relaxed) {
        library.note_pending();
        let now = Instant::now();
        if player.poll(now) {
            library.activate(player.index)?;
            if let Some(next) = player.planned_next() {
                library.prefetch(next);
            }
            deadline = now;
        }
        let frame_index = player.frame;
        let fade = player.opacity(now);
        let frame = &library.current.frames[frame_index];
        let boxes = library.current.boxes;
        let opacity = library.current.opacity;
        let delay = frame.1;
        let (cpu, gpu) = sensors.get();
        let composed = draw_overlay(
            &frame.0,
            cpu,
            gpu,
            &font,
            Overlay {
                boxes,
                opacity,
                color: settings.color,
            },
            fade,
        );
        let composed = rotate_frame(composed, rotation);
        let payload = encode_q565(&composed)?;
        send_q565(&hid, &bulk, endpoint, &payload)?;

        if args.debug {
            sent += 1;
            let now = Instant::now();
            if now.duration_since(window) >= Duration::from_secs(2) {
                let secs = now.duration_since(window).as_secs_f32();
                let image = if player.count > 1 {
                    format!("  image {}/{}", player.index + 1, player.count)
                } else {
                    String::new()
                };
                println!(
                    "{:.1} fps  CPU {}  GPU {}  frame {} KB{image}",
                    sent as f32 / secs,
                    format_temp(cpu, "°C"),
                    format_temp(gpu, "°C"),
                    payload.len() / 1024
                );
                sent = 0;
                window = now;
            }
        }

        deadline += delay;
        let frame_count = library.current.frames.len();
        player.frame = (player.frame + 1) % frame_count;
        let now = Instant::now();
        if deadline > now {
            wait_until(&running, deadline);
        } else {
            deadline = now;
        }
    }
    Ok(())
}

struct LiquidRestore<'a> {
    hid: &'a HidDevice,
}

impl Drop for LiquidRestore<'_> {
    fn drop(&mut self) {
        if hid_write(self.hid, &LIQUID_SCREEN).is_ok() {
            println!("Restored the liquid temperature screen");
        } else {
            println!("Could not restore the liquid temperature screen");
        }
    }
}

fn send_q565(
    hid: &HidDevice,
    bulk: &DeviceHandle<rusb::Context>,
    endpoint: u8,
    payload: &[u8],
) -> Result<()> {
    drain_hid(hid);
    hid_write(hid, &FRAME_SETUP)?;
    let _ = read_hid_prefix(hid, FRAME_SETUP_ACK, 40);

    let header = bulk_header(payload.len());
    bulk.write_bulk(endpoint, &header, Duration::from_secs(1))
        .context("bulk header")?;
    bulk.write_bulk(endpoint, payload, Duration::from_secs(1))
        .context("bulk frame")?;

    hid_write(hid, &FRAME_COMMIT)?;
    let _ = read_hid_prefix(hid, FRAME_COMMIT_ACK, 40);
    Ok(())
}

fn rotate_frame(image: RgbImage, degrees: u16) -> RgbImage {
    match degrees {
        90 => image::imageops::rotate90(&image),
        180 => image::imageops::rotate180(&image),
        270 => image::imageops::rotate270(&image),
        _ => image,
    }
}

fn encode_q565(image: &RgbImage) -> Result<Vec<u8>> {
    let pixels: Vec<u16> = image
        .pixels()
        .map(|Rgb([r, g, b])| encode_rgb565_unchecked(rgb888_to_rgb565([*r, *g, *b])))
        .collect();
    let mut encoded = Vec::with_capacity(160 * 1024);
    let ok = Q565EncodeContext::encode_to_vec(WIDTH as u16, HEIGHT as u16, &pixels, &mut encoded);
    if !ok {
        bail!("Q565 encoder rejected a {WIDTH}x{HEIGHT} frame");
    }
    Ok(encoded)
}

fn open_kraken(usb: &rusb::Context) -> Result<DeviceHandle<rusb::Context>> {
    for device in usb.devices().context("USB device list")?.iter() {
        let Ok(descriptor) = device.device_descriptor() else {
            continue;
        };
        if descriptor.vendor_id() == NZXT_VID && descriptor.product_id() == ELITE_PID {
            return device.open().map_err(usb_busy);
        }
    }
    bail!("Kraken Elite (1e71:300c) was not found")
}

fn claim_bulk_out(handle: &mut DeviceHandle<rusb::Context>) -> Result<u8> {
    let config = handle
        .device()
        .active_config_descriptor()
        .context("USB configuration")?;
    for interface in config.interfaces() {
        for descriptor in interface.descriptors() {
            for endpoint in descriptor.endpoint_descriptors() {
                if endpoint.direction() != Direction::Out
                    || endpoint.transfer_type() != TransferType::Bulk
                {
                    continue;
                }
                let number = interface.number();
                if handle.kernel_driver_active(number).unwrap_or(false) {
                    handle.detach_kernel_driver(number).map_err(usb_busy)?;
                }
                handle.claim_interface(number).map_err(usb_busy)?;
                return Ok(endpoint.address());
            }
        }
    }
    bail!("Kraken bulk OUT endpoint was not found")
}

fn hid_write(device: &HidDevice, report: &[u8]) -> Result<()> {
    let mut buffer = [0u8; 64];
    buffer[..report.len()].copy_from_slice(report);
    device.write(&buffer).context("HID write")?;
    Ok(())
}

fn drain_hid(device: &HidDevice) {
    let mut buffer = [0u8; 64];
    while device.read_timeout(&mut buffer, 0).unwrap_or(0) > 0 {}
}

fn wait_until(running: &AtomicBool, deadline: Instant) {
    while running.load(Ordering::Relaxed) {
        let now = Instant::now();
        if deadline <= now {
            return;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
}

fn reset_liquid() -> Result<()> {
    let hid = open_hid("Stop kraken-gif-and-overlay before --reset")?;
    hid_write(&hid.device, &LIQUID_SCREEN)?;
    println!("Restored the liquid temperature screen");
    Ok(())
}

struct OpenHid {
    _api: hidapi::HidApi,
    device: HidDevice,
}

fn open_hid(purpose: &str) -> Result<OpenHid> {
    let api = hidapi::HidApi::new()?;
    let device = api.open(NZXT_VID, ELITE_PID).map_err(hid_busy).context(purpose.to_string())?;
    Ok(OpenHid { _api: api, device })
}

fn lcd_info(device: &HidDevice) -> Result<(u8, u8)> {
    drain_hid(device);
    hid_write(device, &LCD_QUERY)?;
    let msg = read_hid_prefix(device, LCD_INFO_PREFIX, 500)?;
    let orientation = msg[LCD_ORIENTATION_OFFSET];
    orientation_degrees(orientation)?;
    Ok((msg[LCD_BRIGHTNESS_OFFSET], orientation))
}

fn show_rotation() -> Result<()> {
    let hid = open_hid("Stop kraken-gif-and-overlay before --get-rotation")?;
    let (_, orientation) = lcd_info(&hid.device)?;
    println!("rotation = {}", orientation_degrees(orientation)?);
    Ok(())
}

fn set_rotation(degrees: u16) -> Result<()> {
    let hid = open_hid("Stop kraken-gif-and-overlay before --set-rotation")?;
    let (brightness, _) = lcd_info(&hid.device)?;
    hid_write(&hid.device, &orientation_report(brightness, degrees))?;
    println!("rotation = {degrees}");
    Ok(())
}

fn read_hid_prefix(device: &HidDevice, prefix: [u8; 2], timeout_ms: u64) -> Result<[u8; 64]> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut buffer = [0u8; 64];
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now()).as_millis() as i32;
        match device.read_timeout(&mut buffer, left.max(1)) {
            Ok(n) if n >= 2 && buffer[0] == prefix[0] && buffer[1] == prefix[1] => return Ok(buffer),
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    bail!("The Kraken did not answer.")
}

#[derive(Default)]
struct Args {
    any: bool,
    help: bool,
    debug: bool,
    list_sensors: bool,
    reset: bool,
    get_rotation: bool,
    set_rotation: Option<u16>,
    save_config: bool,
    add: bool,
    delete: bool,
    update: bool,
    gif: Option<PathBuf>,
    position: Option<u8>,
    duration: Option<f64>,
    fade: Option<f64>,
    order: Option<PlayOrder>,
    cpu_sensor: Option<String>,
    gpu_sensor: Option<String>,
    boxes: Option<bool>,
    opacity: Option<u8>,
    color: Option<[u8; 3]>,
    font: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(input: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut args = Args::default();
    let mut rest = input.into_iter();
    while let Some(arg) = rest.next() {
        args.any = true;
        match arg.as_str() {
            "--help" => args.help = true,
            "--debug" => args.debug = true,
            "--list-sensors" => args.list_sensors = true,
            "--reset" => args.reset = true,
            "--get-rotation" => args.get_rotation = true,
            "--set-rotation" => {
                args.set_rotation = Some(parse_rotation(&next_arg(
                    &mut rest,
                    "--set-rotation needs 0, 90, 180, or 270",
                )?)?)
            }
            "--save-config" => args.save_config = true,
            "--add" => args.add = true,
            "--delete" => args.delete = true,
            "--update" => args.update = true,
            "--gif" => args.gif = Some(PathBuf::from(next_arg(&mut rest, "--gif needs a path")?)),
            "--duration" => {
                args.duration = Some(parse_seconds(
                    &next_arg(&mut rest, "--duration needs a number of seconds")?,
                    "duration",
                    false,
                )?)
            }
            "--fade" => {
                args.fade = Some(parse_seconds(
                    &next_arg(&mut rest, "--fade needs a number of seconds")?,
                    "fade",
                    true,
                )?)
            }
            "--order" => {
                args.order = Some(parse_order(&next_arg(
                    &mut rest,
                    "--order needs sequential or random",
                )?)?)
            }
            "--position" => {
                args.position = Some(parse_position(&next_arg(
                    &mut rest,
                    "--position needs a percentage from 0 to 100",
                )?)?)
            }
            "--cpu-sensor" => args.cpu_sensor = Some(next_arg(&mut rest, "--cpu-sensor needs a sensor id")?),
            "--gpu-sensor" => args.gpu_sensor = Some(next_arg(&mut rest, "--gpu-sensor needs a sensor id")?),
            "--box" => args.boxes = Some(parse_bool(&next_arg(&mut rest, "--box needs yes or no")?)?),
            "--opacity" => {
                args.opacity = Some(parse_opacity(&next_arg(
                    &mut rest,
                    "--opacity needs a number from 0 to 255",
                )?)?)
            }
            "--color" => args.color = Some(parse_color(&next_arg(&mut rest, "--color needs RRGGBB")?)?),
            "--font" => {
                let path = PathBuf::from(next_arg(&mut rest, "--font needs a path")?);
                if !path.is_file() {
                    bail!("Font not found: {}", path.display());
                }
                args.font = Some(path);
            }
            other => bail!("Unknown argument {other}. Pass --help to see the switches."),
        }
    }
    Ok(args)
}

#[derive(Debug)]
enum RotationCmd {
    Show,
    Set(u16),
}

fn rotation_request(args: &Args) -> Result<Option<RotationCmd>> {
    if !args.get_rotation && args.set_rotation.is_none() {
        return Ok(None);
    }
    if args.get_rotation && args.set_rotation.is_some() {
        bail!("--get-rotation and --set-rotation must be used on their own.");
    }
    let other = args.help
        || args.debug
        || args.list_sensors
        || args.reset
        || args.save_config
        || args.add
        || args.delete
        || args.update
        || args.gif.is_some()
        || args.position.is_some()
        || args.duration.is_some()
        || args.fade.is_some()
        || args.order.is_some()
        || args.cpu_sensor.is_some()
        || args.gpu_sensor.is_some()
        || args.boxes.is_some()
        || args.opacity.is_some()
        || args.color.is_some()
        || args.font.is_some();
    if other {
        let name = if args.get_rotation { "--get-rotation" } else { "--set-rotation" };
        bail!("{name} must be used on its own.");
    }
    match args.set_rotation {
        Some(degrees) => Ok(Some(RotationCmd::Set(degrees))),
        None => Ok(Some(RotationCmd::Show)),
    }
}

fn parse_rotation(value: &str) -> Result<u16> {
    match value.trim() {
        "0" => Ok(0),
        "90" => Ok(90),
        "180" => Ok(180),
        "270" => Ok(270),
        _ => bail!("rotation must be 0, 90, 180, or 270"),
    }
}

fn next_arg(args: &mut impl Iterator<Item = String>, message: &str) -> Result<String> {
    match args.next() {
        Some(value) if !value.is_empty() && !value.starts_with('-') => Ok(value),
        _ => bail!("{message}"),
    }
}

fn print_usage() {
    let program = std::env::args().next().unwrap_or_else(|| "kraken-gif-and-overlay".to_string());
    println!(
        "\
Usage: {program} [options]

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
  --box <yes|no>         Draw boxes behind the text on this image. Default: no
  --opacity <0-255>      Box opacity for this image. Default: 150
  --color <RRGGBB>       Text colour. Default: f2f2f2
  --font <path>          .ttf font file
  --list-sensors         Print temperature sensors and exit
  --reset                Restore the liquid temperature screen and exit
  --get-rotation         Print the LCD rotation and exit. Use on its own
  --set-rotation <deg>   Set the LCD rotation to 0, 90, 180, or 270, then exit. Use on its own
  --debug                Print each GIF as it loads, and frame stats about every two seconds
  --help                 Show this help
"
    );
}

const CONFIG_FILE: &str = "config.yml";

fn config_exists() -> Result<bool> {
    Ok(data_dir()?.join(CONFIG_FILE).is_file())
}

fn load_gif(path: &Path, position: u8) -> Result<Vec<(RgbImage, Duration)>> {
    let file = File::open(path).with_context(|| format!("GIF not found: {}", path.display()))?;
    let decoder = image::codecs::gif::GifDecoder::new(BufReader::new(file)).context("GIF decoder")?;
    let frames = decoder.into_frames().collect_frames().context("GIF frames")?;
    if frames.is_empty() {
        bail!("GIF has no frames");
    }
    let mut loaded = Vec::with_capacity(frames.len());
    for frame in frames {
        let (numer, denom) = frame.delay().numer_denom_ms();
        let delay = frame_delay(numer, denom);
        let rgb = image::DynamicImage::ImageRgba8(frame.into_buffer()).to_rgb8();
        let (width, height) = rgb.dimensions();
        if width == 0 || height == 0 {
            bail!("GIF frame has no pixels");
        }
        let square = crop_square(&rgb, position);
        let scaled = image::imageops::resize(&square, WIDTH, HEIGHT, FilterType::Lanczos3);
        loaded.push((scaled, delay));
    }
    Ok(loaded)
}

fn frame_delay(numer: u32, denom: u32) -> Duration {
    let millis = if denom == 0 { 100 } else { numer / denom.max(1) };
    Duration::from_millis(u64::from(millis.max(1)))
}

/// Square window on the shorter side. `position` slides it along the longer side.
fn square_origin(width: u32, height: u32, position: u8) -> (u32, u32, u32) {
    let side = width.min(height);
    let span = width.max(height) - side;
    let offset = (u64::from(span) * u64::from(position) / 100) as u32;
    if width >= height {
        (offset, 0, side)
    } else {
        (0, offset, side)
    }
}

fn crop_square(image: &RgbImage, position: u8) -> RgbImage {
    let (width, height) = image.dimensions();
    let (x, y, side) = square_origin(width, height, position);
    image::imageops::crop_imm(image, x, y, side, side).to_image()
}

fn load_font(path: &Path) -> Result<FontArc> {
    let bytes = std::fs::read(path).with_context(|| format!("Font not found: {}", path.display()))?;
    FontArc::try_from_vec(bytes).with_context(|| format!("Font could not be read: {}", path.display()))
}

fn draw_overlay(
    frame: &RgbImage,
    cpu: Option<f32>,
    gpu: Option<f32>,
    font: &FontArc,
    overlay: Overlay,
    fade: f32,
) -> RgbImage {
    let mut image = frame.clone();
    apply_opacity(&mut image, fade);
    let temp_scale = PxScale::from(TEMP_PX);
    let label_scale = PxScale::from(LABEL_PX);
    let outer_pad = 28i32;
    let inner_pad = 16i32;
    let pad_y = 24i32;
    let line_gap = 2i32;
    let column_gap = 36i32;
    let temp_h = TEMP_PX as i32;
    let label_h = LABEL_PX as i32;
    let height = pad_y + temp_h + line_gap + label_h + pad_y;
    let y = (HEIGHT as i32 - height) / 2;
    let center = WIDTH as i32 / 2;
    let ink = Rgb(overlay.color);
    let columns = [
        ("CPU", overlay_temp(cpu), true),
        ("GPU", overlay_temp(gpu), false),
    ];
    for (label, value, toward_center_right) in columns {
        let temp_ink = text_ink(font, temp_scale, &value);
        let label_ink = text_ink(font, label_scale, label);
        let (temp_left, temp_right, temp_x) = if toward_center_right {
            let temp_right = center - column_gap / 2;
            let temp_left = temp_right - temp_ink.width();
            (temp_left, temp_right, temp_ink.pen_x_right(temp_right))
        } else {
            let temp_left = center + column_gap / 2;
            let temp_right = temp_left + temp_ink.width();
            (temp_left, temp_right, temp_ink.pen_x_left(temp_left))
        };
        let label_left = temp_left + (temp_right - temp_left - label_ink.width()) / 2;
        let label_x = label_ink.pen_x_left(label_left);
        let content_left = temp_left.min(label_left);
        let content_right = temp_right.max(label_left + label_ink.width());
        let (x, width) = if toward_center_right {
            let x = content_left - outer_pad;
            (x, content_right + inner_pad - x)
        } else {
            let x = content_left - inner_pad;
            (x, content_right + outer_pad - x)
        };
        if overlay.boxes && overlay.opacity > 0 {
            fill_rect(&mut image, x, y, width, height, [0, 0, 0], overlay.opacity);
        }
        draw_text_mut(&mut image, ink, temp_x, y + pad_y, temp_scale, font, &value);
        draw_text_mut(
            &mut image,
            ink,
            label_x,
            y + pad_y + temp_h + line_gap,
            label_scale,
            font,
            label,
        );
    }
    image
}

struct TextInk {
    min_x: f32,
    max_x: f32,
}

impl TextInk {
    fn width(&self) -> i32 {
        (self.max_x - self.min_x).ceil() as i32
    }

    fn pen_x_left(&self, ink_left: i32) -> i32 {
        ink_left - self.min_x.round() as i32
    }

    fn pen_x_right(&self, ink_right: i32) -> i32 {
        ink_right - self.width() - self.min_x.round() as i32
    }
}

fn text_ink(font: &FontArc, scale: PxScale, text: &str) -> TextInk {
    let scaled = ab_glyph::PxScaleFont { font, scale };
    let mut pen_x = 0.0;
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    for ch in text.chars() {
        let mut glyph = scaled.scaled_glyph(ch);
        glyph.position.x = pen_x;
        let bounds = scaled.glyph_bounds(&glyph);
        if bounds.min.x < bounds.max.x {
            min_x = min_x.min(bounds.min.x);
            max_x = max_x.max(bounds.max.x);
        }
        pen_x += scaled.h_advance(scaled.glyph_id(ch));
    }
    if min_x > max_x {
        TextInk { min_x: 0.0, max_x: pen_x }
    } else {
        TextInk { min_x, max_x }
    }
}

fn fill_rect(image: &mut RgbImage, x: i32, y: i32, width: i32, height: i32, color: [u8; 3], alpha: u8) {
    let alpha = alpha as u16;
    for py in y..(y + height) {
        for px in x..(x + width) {
            if px < 0 || py < 0 || px >= WIDTH as i32 || py >= HEIGHT as i32 {
                continue;
            }
            let pixel = image.get_pixel_mut(px as u32, py as u32);
            for (channel, ink) in pixel.0.iter_mut().zip(color) {
                let src = *channel as u16;
                *channel = ((ink as u16 * alpha + src * (255 - alpha)) / 255) as u8;
            }
        }
    }
}

struct SensorCache {
    cpu: PathBuf,
    gpu: PathBuf,
    cpu_c: Option<f32>,
    gpu_c: Option<f32>,
    read_at: Option<Instant>,
}

impl SensorCache {
    fn get(&mut self) -> (Option<f32>, Option<f32>) {
        let due = self
            .read_at
            .map(|at| at.elapsed() >= Duration::from_secs(1))
            .unwrap_or(true);
        if due {
            self.cpu_c = read_millicelsius(&self.cpu);
            self.gpu_c = read_millicelsius(&self.gpu);
            self.read_at = Some(Instant::now());
        }
        (self.cpu_c, self.gpu_c)
    }
}

const TEMP_PX: f32 = 156.0;
const LABEL_PX: f32 = 72.0;

#[derive(Clone, Copy)]
struct Overlay {
    boxes: bool,
    opacity: u8,
    color: [u8; 3],
}

struct Settings {
    images: Vec<RunnableImage>,
    font: PathBuf,
    color: [u8; 3],
    cpu: PathBuf,
    gpu: PathBuf,
    hold: Duration,
    fade: Duration,
    order: PlayOrder,
}

#[derive(Clone)]
struct RunnableImage {
    path: PathBuf,
    position: u8,
    boxes: bool,
    opacity: u8,
}

struct Slide {
    frames: Vec<(RgbImage, Duration)>,
    boxes: bool,
    opacity: u8,
}

struct LoadReport {
    frames: usize,
    decoded: u64,
}

struct Pending {
    index: usize,
    handle: std::thread::JoinHandle<Result<Slide>>,
    report: Option<std::sync::mpsc::Receiver<LoadReport>>,
    reported: bool,
}

/// The image on screen, plus the next one decoding on a side thread.
struct Library {
    images: Vec<RunnableImage>,
    current: Slide,
    pending: Option<Pending>,
    debug: bool,
}

impl Library {
    fn open(images: Vec<RunnableImage>, index: usize, debug: bool) -> Result<Self> {
        note_loading(debug, &images[index].path);
        let current = load_slide(&images[index])?;
        note_loaded(debug, current.frames.len(), slide_bytes(&current));
        Ok(Self {
            images,
            current,
            pending: None,
            debug,
        })
    }

    fn prefetch(&mut self, index: usize) {
        if self.pending.as_ref().is_some_and(|job| job.index == index) {
            return;
        }
        if let Some(job) = self.pending.take() {
            let _ = job.handle.join();
        }
        let image = self.images[index].clone();
        note_loading(self.debug, &image.path);
        let debug = self.debug;
        let (sender, report) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let slide = load_slide(&image)?;
            if debug {
                let _ = sender.send(LoadReport {
                    frames: slide.frames.len(),
                    decoded: slide_bytes(&slide),
                });
            }
            Ok(slide)
        });
        self.pending = Some(Pending {
            index,
            handle,
            report: debug.then_some(report),
            reported: false,
        });
    }

    /// Print the size of a prefetch once its decode has finished.
    fn note_pending(&mut self) {
        let Some(job) = self.pending.as_mut() else {
            return;
        };
        if job.reported {
            return;
        }
        let Some(report) = job.report.as_ref() else {
            return;
        };
        if let Ok(report) = report.try_recv() {
            job.reported = true;
            note_loaded(true, report.frames, report.decoded);
        }
    }

    fn activate(&mut self, index: usize) -> Result<()> {
        let debug = self.debug;
        let slide = match self.pending.take() {
            Some(job) if job.index == index => {
                let slide = join_loader(job.handle)?;
                if !job.reported {
                    note_loaded(debug, slide.frames.len(), slide_bytes(&slide));
                }
                slide
            }
            Some(job) => {
                let _ = job.handle.join();
                note_loading(debug, &self.images[index].path);
                let slide = load_slide(&self.images[index])?;
                note_loaded(debug, slide.frames.len(), slide_bytes(&slide));
                slide
            }
            None => {
                note_loading(debug, &self.images[index].path);
                let slide = load_slide(&self.images[index])?;
                note_loaded(debug, slide.frames.len(), slide_bytes(&slide));
                slide
            }
        };
        self.current = slide;
        Ok(())
    }
}

fn join_loader(handle: std::thread::JoinHandle<Result<Slide>>) -> Result<Slide> {
    match handle.join() {
        Ok(slide) => slide,
        Err(_) => bail!("GIF loader stopped"),
    }
}

fn load_slide(image: &RunnableImage) -> Result<Slide> {
    Ok(Slide {
        frames: load_gif(&image.path, image.position)?,
        boxes: image.boxes,
        opacity: image.opacity,
    })
}

fn slide_bytes(slide: &Slide) -> u64 {
    slide.frames.iter().map(|(image, _)| image.as_raw().len() as u64).sum()
}

fn resident_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

fn format_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

fn load_summary(frames: usize, decoded: u64, resident: Option<u64>) -> String {
    let label = if frames == 1 { "frame" } else { "frames" };
    match resident {
        Some(rss) => format!("{frames} {label}, {} decoded, {} resident", format_mb(decoded), format_mb(rss)),
        None => format!("{frames} {label}, {} decoded", format_mb(decoded)),
    }
}

fn note_loading(debug: bool, path: &Path) {
    if debug {
        println!("Loading {}", path.display());
    }
}

fn note_loaded(debug: bool, frames: usize, decoded: u64) {
    if debug {
        println!("{}", load_summary(frames, decoded, resident_bytes()));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    In,
    Hold,
    Out,
}

struct Player {
    count: usize,
    index: usize,
    frame: usize,
    /// Chosen while the current image is still on screen, so its GIF can be decoded ahead of the switch.
    planned: Option<usize>,
    phase: Phase,
    phase_start: Instant,
    hold: Duration,
    fade: Duration,
    order: PlayOrder,
    rng: Rng,
}

impl Player {
    fn new(count: usize, hold: Duration, fade: Duration, order: PlayOrder) -> Self {
        let fade_in = count > 1 && !fade.is_zero();
        let mut rng = Rng::from_time();
        let index = starting_slide(order, count, &mut rng);
        Self {
            count,
            index,
            frame: 0,
            planned: None,
            phase: if fade_in { Phase::In } else { Phase::Hold },
            phase_start: Instant::now(),
            hold,
            fade,
            order,
            rng,
        }
    }

    /// The image to decode during this hold. The choice stays put until the switch.
    fn planned_next(&mut self) -> Option<usize> {
        if self.count < 2 {
            return None;
        }
        if self.planned.is_none() {
            self.planned = Some(next_slide(self.order, self.index, self.count, &mut self.rng));
        }
        self.planned
    }

    /// Advance the fade and, when the hold is over, the current image.
    /// Returns true when the image changed.
    fn poll(&mut self, now: Instant) -> bool {
        if self.count < 2 || (self.hold.is_zero() && self.fade.is_zero()) {
            return false;
        }
        let mut switched = false;
        loop {
            let elapsed = now.saturating_duration_since(self.phase_start);
            match self.phase {
                Phase::In if self.fade.is_zero() || elapsed >= self.fade => {
                    self.phase = Phase::Hold;
                    self.phase_start = now;
                }
                Phase::Hold if elapsed >= self.hold => {
                    if self.fade.is_zero() {
                        self.advance(now);
                        switched = true;
                    } else {
                        self.phase = Phase::Out;
                        self.phase_start = now;
                    }
                }
                Phase::Out if elapsed >= self.fade => {
                    self.advance(now);
                    switched = true;
                }
                _ => break,
            }
        }
        switched
    }

    fn advance(&mut self, now: Instant) {
        self.index = match self.planned.take() {
            Some(index) => index,
            None => next_slide(self.order, self.index, self.count, &mut self.rng),
        };
        self.frame = 0;
        self.phase = if self.fade.is_zero() { Phase::Hold } else { Phase::In };
        self.phase_start = now;
    }

    fn opacity(&self, now: Instant) -> f32 {
        if self.count < 2 {
            return 1.0;
        }
        phase_opacity(self.phase, now.saturating_duration_since(self.phase_start), self.fade)
    }
}

fn phase_opacity(phase: Phase, elapsed: Duration, fade: Duration) -> f32 {
    if fade.is_zero() {
        return 1.0;
    }
    let ratio = elapsed.as_secs_f32() / fade.as_secs_f32();
    match phase {
        Phase::In => ratio.clamp(0.0, 1.0),
        Phase::Hold => 1.0,
        Phase::Out => (1.0 - ratio).clamp(0.0, 1.0),
    }
}

fn starting_slide(order: PlayOrder, len: usize, rng: &mut Rng) -> usize {
    if len <= 1 || order == PlayOrder::Sequential {
        return 0;
    }
    (rng.next_u64() as usize) % len
}

fn next_slide(order: PlayOrder, current: usize, len: usize, rng: &mut Rng) -> usize {
    if len <= 1 {
        return 0;
    }
    match order {
        PlayOrder::Sequential => (current + 1) % len,
        PlayOrder::Random => {
            let pick = (rng.next_u64() as usize) % (len - 1);
            if pick < current { pick } else { pick + 1 }
        }
    }
}

fn apply_opacity(image: &mut RgbImage, opacity: f32) {
    if opacity >= 1.0 {
        return;
    }
    let factor = opacity.clamp(0.0, 1.0);
    for pixel in image.pixels_mut() {
        for channel in &mut pixel.0 {
            *channel = (*channel as f32 * factor).round() as u8;
        }
    }
}

struct Rng(u64);

impl Rng {
    fn from_time() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0x5EED);
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

const DEFAULT_DURATION: f64 = 120.0;
const DEFAULT_FADE: f64 = 0.3;
const DEFAULT_OPACITY: u8 = 150;
const DEFAULT_POSITION: u8 = 50;

fn default_duration() -> f64 {
    DEFAULT_DURATION
}

fn default_fade() -> f64 {
    DEFAULT_FADE
}

fn default_box_opacity() -> u8 {
    DEFAULT_OPACITY
}

fn default_position() -> u8 {
    DEFAULT_POSITION
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PlayOrder {
    #[default]
    Sequential,
    Random,
}

impl PlayOrder {
    fn as_str(self) -> &'static str {
        match self {
            PlayOrder::Sequential => "sequential",
            PlayOrder::Random => "random",
        }
    }
}

impl Serialize for PlayOrder {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PlayOrder {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_order(&text).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    sensors: SensorConfig,
    #[serde(default)]
    overlay: OverlayConfig,
    #[serde(default)]
    slideshow: SlideshowConfig,
    #[serde(default)]
    images: Vec<ImageConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SensorConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cpu: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gpu: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct OverlayConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    font: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    color: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SlideshowConfig {
    #[serde(default = "default_duration", serialize_with = "serialize_seconds")]
    duration: f64,
    #[serde(default = "default_fade", serialize_with = "serialize_seconds")]
    fade: f64,
    #[serde(default)]
    order: PlayOrder,
}

impl Default for SlideshowConfig {
    fn default() -> Self {
        Self {
            duration: default_duration(),
            fade: default_fade(),
            order: PlayOrder::Sequential,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageConfig {
    #[serde(rename = "gifPath")]
    gif_path: String,
    #[serde(default, rename = "box")]
    boxes: bool,
    #[serde(default = "default_box_opacity", rename = "boxOpacity")]
    box_opacity: u8,
    #[serde(default = "default_position")]
    position: u8,
}

fn serialize_seconds<S: serde::Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.fract() == 0.0 && value.abs() < i64::MAX as f64 {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}

const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
    "/usr/share/fonts/google-noto/NotoSans-Bold.ttf",
];

struct Sensor {
    id: String,
    chip: String,
    label: String,
    path: PathBuf,
    has_fan: bool,
    celsius: f32,
}

const GPU_CHIPS: &[&str] = &["amdgpu", "nvidia", "nouveau", "i915", "xe"];
const CPU_CHIPS: &[&str] = &["coretemp", "k10temp", "zenpower", "cpu_thermal"];
const CPU_LABELS: &[&str] = &["Tctl", "Tdie", "Package id 0"];

fn load_settings(args: &Args) -> Result<Option<Settings>> {
    let dir = data_dir()?;
    let config_path = dir.join(CONFIG_FILE);
    let text = std::fs::read_to_string(&config_path).unwrap_or_default();
    let mut config = load_config_text(&text)?;
    let sensors = scan_sensors();

    let cpu = temperature_sensor(
        &sensors,
        args.cpu_sensor.as_deref().or(config.sensors.cpu.as_deref()),
        pick_cpu,
        "CPU",
        "cpu-sensor",
    )?;
    let gpu = temperature_sensor(
        &sensors,
        args.gpu_sensor.as_deref().or(config.sensors.gpu.as_deref()),
        pick_gpu,
        "GPU",
        "gpu-sensor",
    )?;
    let cpu_path = cpu.path.clone();
    let gpu_path = gpu.path.clone();

    if args.save_config {
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        apply_save(&dir, &mut config, args, &cpu.id, &gpu.id)?;
        write_config(&config_path, &config)?;
        return Ok(None);
    }

    let images = images_for_run(&dir, &config, args)?;
    if images.len() > 1 && args.duration.unwrap_or(config.slideshow.duration) <= 0.0 {
        bail!("Set slideshow duration above 0 seconds when more than one image is configured.");
    }
    let font = if let Some(path) = &args.font {
        path.clone()
    } else {
        font_path(&dir, config.overlay.font.as_deref())?
    };
    let color = if let Some(color) = args.color {
        color
    } else if let Some(color) = config.overlay.color.as_deref() {
        parse_color(color)?
    } else {
        [242, 242, 242]
    };
    Ok(Some(Settings {
        images,
        font,
        color,
        cpu: cpu_path,
        gpu: gpu_path,
        hold: seconds(args.duration.unwrap_or(config.slideshow.duration), "duration")?,
        fade: seconds(args.fade.unwrap_or(config.slideshow.fade), "fade")?,
        order: args.order.unwrap_or(config.slideshow.order),
    }))
}

fn images_for_run(dir: &Path, config: &Config, args: &Args) -> Result<Vec<RunnableImage>> {
    if args.gif.is_some() && !args.save_config {
        let path = args.gif.clone().context("Pass --gif <path> to configure a GIF.")?;
        if !path.is_file() {
            bail!("GIF not found: {}", path.display());
        }
        return Ok(vec![RunnableImage {
            path,
            position: args.position.unwrap_or(DEFAULT_POSITION),
            boxes: args.boxes.unwrap_or(false),
            opacity: args.opacity.unwrap_or(DEFAULT_OPACITY),
        }]);
    }
    if config.images.is_empty() {
        bail!("Pass --gif <path> to configure a GIF.");
    }
    config
        .images
        .iter()
        .map(|image| {
            let path = in_data_dir(dir, &image.gif_path);
            if !path.is_file() {
                bail!("GIF not found: {}", path.display());
            }
            if image.position > 100 {
                bail!("position must be a percentage from 0 to 100");
            }
            Ok(RunnableImage {
                path,
                position: image.position,
                boxes: image.boxes,
                opacity: image.box_opacity,
            })
        })
        .collect()
}

fn font_path(dir: &Path, configured: Option<&str>) -> Result<PathBuf> {
    if let Some(name) = configured {
        let path = in_data_dir(dir, name);
        if !path.is_file() {
            bail!("Font not found: {}", path.display());
        }
        return Ok(path);
    }
    FONT_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .context("No DejaVu Sans or Noto Sans Bold font was found. Set font in the config to a .ttf file.")
}

fn parse_position(value: &str) -> Result<u8> {
    let text = value.trim().trim_end_matches('%').trim();
    let number: u8 = text
        .parse()
        .context("position must be a percentage from 0 to 100")?;
    if number > 100 {
        bail!("position must be a percentage from 0 to 100");
    }
    Ok(number)
}

fn parse_seconds(value: &str, name: &str, allow_zero: bool) -> Result<f64> {
    let number: f64 = value
        .trim()
        .parse()
        .with_context(|| format!("{name} must be a number of seconds"))?;
    if !number.is_finite() || number < 0.0 || (!allow_zero && number == 0.0) {
        if allow_zero {
            bail!("{name} must be zero seconds or more");
        }
        bail!("{name} must be more than zero seconds");
    }
    Ok(number)
}

fn seconds(value: f64, name: &str) -> Result<Duration> {
    if !value.is_finite() || value < 0.0 {
        bail!("{name} must be a number of seconds");
    }
    Duration::try_from_secs_f64(value).with_context(|| format!("{name} must be a number of seconds"))
}

fn parse_order(value: &str) -> Result<PlayOrder> {
    match value.trim().to_ascii_lowercase().as_str() {
        "sequential" => Ok(PlayOrder::Sequential),
        "random" => Ok(PlayOrder::Random),
        _ => bail!("order must be sequential or random"),
    }
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("box must be yes or no"),
    }
}

fn parse_opacity(value: &str) -> Result<u8> {
    let number: u16 = value
        .parse()
        .context("opacity must be a number from 0 to 255")?;
    u8::try_from(number).context("opacity must be from 0 to 255")
}

fn parse_color(value: &str) -> Result<[u8; 3]> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let channel = |start: usize| u8::from_str_radix(&hex[start..start + 2], 16).unwrap();
        return Ok([channel(0), channel(2), channel(4)]);
    }
    let parts: Vec<_> = value.split(',').map(str::trim).collect();
    if parts.len() == 3 {
        let channel = |text: &str| -> Result<u8> {
            text.parse()
                .context("color channels must be numbers from 0 to 255")
        };
        return Ok([channel(parts[0])?, channel(parts[1])?, channel(parts[2])?]);
    }
    bail!("color must be RRGGBB, such as f2f2f2")
}

fn copy_gif_into(dir: &Path, source: &Path) -> Result<String> {
    if !source.is_file() {
        bail!("GIF not found: {}", source.display());
    }
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .context("GIF file name is not valid text")?;
    let dest = dir.join(name);
    if !same_file(source, &dest) {
        std::fs::copy(source, &dest)
            .with_context(|| format!("copy {} to {}", source.display(), dest.display()))?;
        println!("Copied {}", dest.display());
    }
    Ok(name.to_string())
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn font_config_value(dir: &Path, path: &Path) -> Result<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(relative_to(dir, &absolute))
}

#[derive(Clone, Copy)]
struct SaveFlags {
    gif: bool,
    position: bool,
    boxes: bool,
    opacity: bool,
    add: bool,
    delete: bool,
    update: bool,
}

impl SaveFlags {
    fn from_args(args: &Args) -> Self {
        Self {
            gif: args.gif.is_some(),
            position: args.position.is_some(),
            boxes: args.boxes.is_some(),
            opacity: args.opacity.is_some(),
            add: args.add,
            delete: args.delete,
            update: args.update,
        }
    }

    fn touches_image(self) -> bool {
        self.gif || self.position || self.boxes || self.opacity || self.add || self.delete || self.update
    }
}

#[derive(Debug)]
enum SavePlan {
    GlobalsOnly,
    Add,
    Delete,
    Update,
}

fn save_plan(image_count: usize, flags: SaveFlags) -> Result<SavePlan> {
    let verbs = [flags.add, flags.delete, flags.update].iter().filter(|flag| **flag).count();
    if verbs > 1 {
        bail!("Pass only one of --add, --delete, or --update.");
    }
    if image_count == 0 {
        if flags.delete {
            bail!("No image is configured to delete.");
        }
        if flags.update {
            bail!("No image is configured to update.");
        }
        if !flags.gif {
            bail!("Pass --gif <path> to add an image.");
        }
        return Ok(SavePlan::Add);
    }
    if verbs == 0 {
        if flags.touches_image() {
            if image_count == 1 {
                bail!("An image is already configured. Include --add, --delete, or --update.");
            }
            bail!("Images are already configured. Include --add or --delete.");
        }
        return Ok(SavePlan::GlobalsOnly);
    }
    if flags.update && image_count != 1 {
        bail!("--update only applies when a single image is configured. Include --add or --delete.");
    }
    if flags.add && !flags.gif {
        bail!("Pass --gif <path> to add an image.");
    }
    if flags.add {
        return Ok(SavePlan::Add);
    }
    if flags.delete {
        return Ok(SavePlan::Delete);
    }
    Ok(SavePlan::Update)
}

fn apply_save(dir: &Path, config: &mut Config, args: &Args, cpu_id: &str, gpu_id: &str) -> Result<()> {
    let plan = save_plan(config.images.len(), SaveFlags::from_args(args))?;
    config.sensors.cpu = Some(cpu_id.to_string());
    config.sensors.gpu = Some(gpu_id.to_string());
    if let Some(color) = args.color {
        config.overlay.color = Some(format!("{:02x}{:02x}{:02x}", color[0], color[1], color[2]));
    } else if config.overlay.color.is_none() {
        config.overlay.color = Some("f2f2f2".to_string());
    }
    if let Some(font) = &args.font {
        config.overlay.font = Some(font_config_value(dir, font)?);
    }
    if let Some(duration) = args.duration {
        config.slideshow.duration = duration;
    }
    if let Some(fade) = args.fade {
        config.slideshow.fade = fade;
    }
    if let Some(order) = args.order {
        config.slideshow.order = order;
    }
    match plan {
        SavePlan::GlobalsOnly => {}
        SavePlan::Add => {
            let image = new_image(dir, args, &config.images)?;
            config.images.push(image);
        }
        SavePlan::Delete => delete_image(config, args.gif.as_deref())?,
        SavePlan::Update => update_image(dir, &mut config.images[0], args)?,
    }
    validate_config(config)
}

fn new_image(dir: &Path, args: &Args, existing: &[ImageConfig]) -> Result<ImageConfig> {
    let source = args.gif.as_deref().context("Pass --gif <path> to add an image.")?;
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .context("GIF file name is not valid text")?;
    if existing.iter().any(|image| file_name(&image.gif_path) == name) {
        bail!("{name} is already in the image list. Include --delete to remove it.");
    }
    Ok(ImageConfig {
        gif_path: copy_gif_into(dir, source)?,
        boxes: args.boxes.unwrap_or(false),
        box_opacity: args.opacity.unwrap_or(DEFAULT_OPACITY),
        position: args.position.unwrap_or(DEFAULT_POSITION),
    })
}

fn update_image(dir: &Path, image: &mut ImageConfig, args: &Args) -> Result<()> {
    if let Some(source) = &args.gif {
        image.gif_path = copy_gif_into(dir, source)?;
    }
    if let Some(boxes) = args.boxes {
        image.boxes = boxes;
    }
    if let Some(opacity) = args.opacity {
        image.box_opacity = opacity;
    }
    if let Some(position) = args.position {
        image.position = position;
    }
    Ok(())
}

fn delete_image(config: &mut Config, gif: Option<&Path>) -> Result<()> {
    let Some(gif) = gif else {
        if config.images.len() == 1 {
            config.images.clear();
            return Ok(());
        }
        bail!("Pass --gif <path> to choose which image to delete.");
    };
    let name = gif
        .file_name()
        .and_then(|name| name.to_str())
        .context("GIF file name is not valid text")?;
    let before = config.images.len();
    config.images.retain(|image| {
        file_name(&image.gif_path) != name && image.gif_path != gif.display().to_string()
    });
    if config.images.len() == before {
        bail!("No configured image matches {name}.");
    }
    Ok(())
}

fn file_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

fn write_config(path: &Path, config: &Config) -> Result<()> {
    let mut text = serde_yaml::to_string(config).context("Could not write config")?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    std::fs::write(path, &text).with_context(|| format!("write {}", path.display()))?;
    println!("Wrote {}", path.display());
    println!("Restart the service for the change to apply:");
    println!("  sudo systemctl restart kraken-gif-and-overlay");
    Ok(())
}

fn load_config_text(text: &str) -> Result<Config> {
    if text.trim().is_empty() {
        return Ok(Config::default());
    }
    let config = serde_yaml::from_str(text).context("Could not read config")?;
    validate_config(&config)?;
    Ok(config)
}

fn validate_config(config: &Config) -> Result<()> {
    if !config.slideshow.duration.is_finite() || config.slideshow.duration < 0.0 {
        bail!("duration must be a number of seconds");
    }
    if !config.slideshow.fade.is_finite() || config.slideshow.fade < 0.0 {
        bail!("fade must be a number of seconds");
    }
    for image in &config.images {
        if image.gif_path.trim().is_empty() {
            bail!("gifPath is empty");
        }
        if image.position > 100 {
            bail!("position must be a percentage from 0 to 100");
        }
    }
    if let Some(color) = &config.overlay.color {
        parse_color(color)?;
    }
    Ok(())
}

fn relative_to(dir: &Path, path: &Path) -> String {
    path.strip_prefix(dir).unwrap_or(path).display().to_string()
}

fn in_data_dir(dir: &Path, name: &str) -> PathBuf {
    let path = PathBuf::from(name);
    if path.is_absolute() { path } else { dir.join(path) }
}

fn data_dir() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(xdg).join("kraken-gif-and-overlay"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/share/kraken-gif-and-overlay"))
}

fn print_sensors(sensors: &[Sensor]) {
    println!("Temperature sensors (id, reading, fan, sysfs):");
    if sensors.is_empty() {
        println!("  none found under /sys/class/hwmon");
    }
    for sensor in sensors {
        let fan = if sensor.has_fan { "fan" } else { "   " };
        println!(
            "  {:<36} {:>5.0}°C  {fan}  {}",
            sensor.id,
            sensor.celsius,
            sensor.path.display()
        );
    }
    let cpu = pick_cpu(sensors).map(|sensor| sensor.id.as_str()).unwrap_or("none");
    let gpu = pick_gpu(sensors).map(|sensor| sensor.id.as_str()).unwrap_or("none");
    println!("cpu = {cpu}");
    println!("gpu = {gpu}");
}

fn scan_sensors() -> Vec<Sensor> {
    let mut sensors = Vec::new();
    let Ok(nodes) = std::fs::read_dir("/sys/class/hwmon") else {
        return sensors;
    };
    for node in nodes.flatten() {
        let path = node.path();
        let Ok(chip) = std::fs::read_to_string(path.join("name")) else {
            continue;
        };
        let chip = chip.trim().to_string();
        let has_fan = std::fs::read_dir(&path)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with("fan") && name.ends_with("_input")
            });
        let suffix = device_suffix(&path);
        for index in 1..=16 {
            let input = path.join(format!("temp{index}_input"));
            let Some(celsius) = read_millicelsius(&input) else {
                continue;
            };
            let label = std::fs::read_to_string(path.join(format!("temp{index}_label")))
                .ok()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| format!("temp{index}"));
            let id = match &suffix {
                Some(suffix) => format!("{chip}:{label}@{suffix}"),
                None => format!("{chip}:{label}"),
            };
            sensors.push(Sensor {
                id,
                chip: chip.clone(),
                label,
                path: input,
                has_fan,
                celsius,
            });
        }
    }
    sensors.sort_by(|left, right| left.id.cmp(&right.id));
    uniquify_ids(&mut sensors);
    sensors.sort_by(|left, right| left.id.cmp(&right.id));
    sensors
}

fn device_suffix(hwmon: &Path) -> Option<String> {
    let device = std::fs::read_link(hwmon.join("device")).ok()?;
    let last = device.components().next_back()?.as_os_str().to_string_lossy();
    if is_i2c_address(&last) {
        return Some(last.into_owned());
    }
    device
        .components()
        .rev()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .find(|part| is_pci_address(part))
}

fn is_pci_address(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 12
        && bytes[4] == b':'
        && bytes[7] == b':'
        && bytes[10] == b'.'
        && bytes[..4].iter().all(u8::is_ascii_hexdigit)
        && bytes[5..7].iter().all(u8::is_ascii_hexdigit)
        && bytes[8..10].iter().all(u8::is_ascii_hexdigit)
        && bytes[11].is_ascii_hexdigit()
}

fn is_i2c_address(text: &str) -> bool {
    let Some((bus, addr)) = text.split_once('-') else {
        return false;
    };
    !bus.is_empty()
        && bus.bytes().all(|byte| byte.is_ascii_digit())
        && addr.len() == 4
        && addr.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn uniquify_ids(sensors: &mut [Sensor]) {
    let mut counts = std::collections::HashMap::<String, usize>::new();
    for sensor in sensors.iter() {
        *counts.entry(sensor.id.clone()).or_default() += 1;
    }
    for sensor in sensors.iter_mut() {
        if counts[&sensor.id] > 1 {
            if let Some(stem) = sensor.path.file_stem().and_then(|stem| stem.to_str()) {
                let stem = stem.trim_end_matches("_input");
                sensor.id = format!("{}@{stem}", sensor.id);
            }
        }
    }
}

fn pick_cpu(sensors: &[Sensor]) -> Option<&Sensor> {
    let cpus: Vec<_> = sensors
        .iter()
        .filter(|sensor| CPU_CHIPS.contains(&sensor.chip.as_str()))
        .collect();
    for label in CPU_LABELS {
        if let Some(sensor) = cpus.iter().copied().find(|sensor| sensor.label == *label) {
            return Some(sensor);
        }
    }
    cpus.first().copied()
}

fn pick_gpu(sensors: &[Sensor]) -> Option<&Sensor> {
    let gpus: Vec<_> = sensors
        .iter()
        .filter(|sensor| GPU_CHIPS.contains(&sensor.chip.as_str()))
        .collect();
    let with_fan: Vec<_> = gpus.iter().copied().filter(|sensor| sensor.has_fan).collect();
    let pool: Vec<_> = if with_fan.is_empty() { gpus } else { with_fan };
    pool.iter()
        .copied()
        .find(|sensor| sensor.path.ends_with("temp1_input"))
        .or_else(|| pool.first().copied())
}

fn temperature_sensor<'a>(
    sensors: &'a [Sensor],
    spec: Option<&str>,
    pick: fn(&[Sensor]) -> Option<&Sensor>,
    label: &str,
    flag: &str,
) -> Result<&'a Sensor> {
    match spec {
        Some(spec) => resolve_sensor(sensors, spec),
        None => pick(sensors).with_context(|| {
            format!("No {label} temperature matched. Run with --list-sensors, then pass --{flag}.")
        }),
    }
}

fn resolve_sensor<'a>(sensors: &'a [Sensor], spec: &str) -> Result<&'a Sensor> {
    if let Some(sensor) = sensors.iter().find(|sensor| sensor.id == spec) {
        return Ok(sensor);
    }
    let matches: Vec<_> = sensors
        .iter()
        .filter(|sensor| sensor.id.starts_with(&format!("{spec}@")) || format!("{}:{}", sensor.chip, sensor.label) == spec)
        .collect();
    match matches.len() {
        1 => Ok(matches[0]),
        0 => bail!("Unknown sensor {spec}. Run with --list-sensors to see the ids."),
        _ => bail!("Sensor {spec} matches more than one device. Use the full id, including @."),
    }
}

fn read_millicelsius(path: &Path) -> Option<f32> {
    let raw = std::fs::read_to_string(path).ok()?;
    raw.trim().parse::<f32>().ok().map(|value| value / 1000.0)
}

fn usb_busy(err: rusb::Error) -> anyhow::Error {
    match err {
        rusb::Error::Busy => anyhow::anyhow!("The Kraken is in use by another program"),
        rusb::Error::Access => anyhow::anyhow!(
            "The Kraken could not be opened. Another program has it, or this user cannot open it."
        ),
        other => anyhow::anyhow!("USB: {other}"),
    }
}

fn hid_busy(err: hidapi::HidError) -> anyhow::Error {
    let text = err.to_string();
    let lower = text.to_ascii_lowercase();
    if lower.contains("busy") || lower.contains("resource") {
        anyhow::anyhow!("The Kraken is in use by another program")
    } else {
        anyhow::anyhow!(text)
    }
}

fn overlay_temp(value: Option<f32>) -> String {
    format_temp(value, "°")
}

fn format_temp(value: Option<f32>, suffix: &str) -> String {
    match value {
        Some(value) => format!("{}{suffix}", value.round() as i32),
        None => "--".to_string(),
    }
}

#[cfg(test)]
mod crop {
    use super::*;

    #[test]
    fn position_slides_the_square_along_the_long_side() {
        let mut wide = RgbImage::from_pixel(6, 2, Rgb([0, 0, 0]));
        wide.put_pixel(0, 0, Rgb([1, 0, 0]));
        wide.put_pixel(2, 0, Rgb([0, 1, 0]));
        wide.put_pixel(4, 0, Rgb([0, 0, 1]));
        assert_eq!(*crop_square(&wide, 0).get_pixel(0, 0), Rgb([1, 0, 0]));
        assert_eq!(*crop_square(&wide, 50).get_pixel(0, 0), Rgb([0, 1, 0]));
        assert_eq!(*crop_square(&wide, 100).get_pixel(0, 0), Rgb([0, 0, 1]));
        assert_eq!(crop_square(&wide, 100).dimensions(), (2, 2));

        let mut tall = RgbImage::from_pixel(2, 6, Rgb([0, 0, 0]));
        tall.put_pixel(0, 0, Rgb([1, 0, 0]));
        tall.put_pixel(0, 4, Rgb([0, 0, 1]));
        assert_eq!(*crop_square(&tall, 0).get_pixel(0, 0), Rgb([1, 0, 0]));
        assert_eq!(*crop_square(&tall, 100).get_pixel(0, 0), Rgb([0, 0, 1]));
    }

    #[test]
    fn square_gif_ignores_position() {
        let image = RgbImage::from_pixel(4, 4, Rgb([9, 9, 9]));
        assert_eq!(square_origin(4, 4, 0), (0, 0, 4));
        assert_eq!(square_origin(4, 4, 100), (0, 0, 4));
        assert_eq!(crop_square(&image, 100).dimensions(), (4, 4));
    }

    #[test]
    fn position_accepts_a_percent_sign() {
        assert_eq!(parse_position("0").unwrap(), 0);
        assert_eq!(parse_position("50%").unwrap(), 50);
        assert_eq!(parse_position("100").unwrap(), 100);
        assert!(parse_position("101").is_err());
        assert!(parse_position("left").is_err());
    }

    #[test]
    fn a_missing_gif_delay_is_a_tenth_of_a_second() {
        assert_eq!(frame_delay(0, 0), Duration::from_millis(100));
        assert_eq!(frame_delay(50, 1), Duration::from_millis(50));
        assert_eq!(frame_delay(0, 1), Duration::from_millis(1));
    }
}

#[cfg(test)]
mod slideshow {
    use super::*;

    fn flags(gif: bool, add: bool, delete: bool, update: bool) -> SaveFlags {
        SaveFlags {
            gif,
            position: false,
            boxes: false,
            opacity: false,
            add,
            delete,
            update,
        }
    }

    #[test]
    fn first_image_is_added_and_later_ones_need_a_verb() {
        assert!(matches!(save_plan(0, flags(true, false, false, false)).unwrap(), SavePlan::Add));

        let one = format!("{}", save_plan(1, flags(true, false, false, false)).unwrap_err());
        assert!(one.contains("--add"));
        assert!(one.contains("--delete"));
        assert!(one.contains("--update"));

        let many = format!("{}", save_plan(2, flags(true, false, false, false)).unwrap_err());
        assert!(many.contains("--add"));
        assert!(many.contains("--delete"));
        assert!(!many.contains("--update"));

        let update = format!("{}", save_plan(2, flags(false, false, false, true)).unwrap_err());
        assert!(update.contains("--update"));
        assert!(matches!(save_plan(2, flags(false, false, false, false)).unwrap(), SavePlan::GlobalsOnly));
        assert!(matches!(save_plan(1, flags(true, true, false, false)).unwrap(), SavePlan::Add));
    }

    #[test]
    fn order_walks_in_sequence_or_picks_a_different_image() {
        let mut rng = Rng(1);
        assert_eq!(next_slide(PlayOrder::Sequential, 0, 3, &mut rng), 1);
        assert_eq!(next_slide(PlayOrder::Sequential, 2, 3, &mut rng), 0);
        assert_eq!(next_slide(PlayOrder::Random, 0, 2, &mut rng), 1);
        for _ in 0..40 {
            let next = next_slide(PlayOrder::Random, 1, 4, &mut rng);
            assert_ne!(next, 1);
            assert!(next < 4);
        }
        let mut opened_elsewhere = false;
        for seed in 1..30 {
            let start = starting_slide(PlayOrder::Random, 4, &mut Rng(seed));
            assert!(start < 4);
            opened_elsewhere |= start != 0;
        }
        assert!(opened_elsewhere);
        assert_eq!(starting_slide(PlayOrder::Sequential, 4, &mut Rng(1)), 0);
    }

    #[test]
    fn the_hold_advances_a_sequential_slideshow() {
        let mut player = Player::new(2, Duration::from_millis(30), Duration::ZERO, PlayOrder::Sequential);
        let start = player.phase_start;
        assert_eq!(player.index, 0);
        assert!(!player.poll(start + Duration::from_millis(29)));
        assert!(player.poll(start + Duration::from_millis(30)));
        assert_eq!(player.index, 1);
    }

    #[test]
    fn the_prefetched_index_is_the_image_that_plays_next() {
        let mut player = Player::new(4, Duration::from_millis(10), Duration::ZERO, PlayOrder::Sequential);
        assert_eq!(player.planned_next(), Some(1));
        assert_eq!(player.planned_next(), Some(1));
        let start = player.phase_start;
        assert!(player.poll(start + Duration::from_millis(10)));
        assert_eq!(player.index, 1);
        assert_eq!(player.planned_next(), Some(2));

        let mut random = Player::new(4, Duration::from_millis(10), Duration::ZERO, PlayOrder::Random);
        let next = random.planned_next().unwrap();
        assert_ne!(next, random.index);
        let start = random.phase_start;
        assert!(random.poll(start + Duration::from_millis(10)));
        assert_eq!(random.index, next);

        let mut single = Player::new(1, Duration::from_secs(1), Duration::from_millis(300), PlayOrder::Sequential);
        assert!(single.planned_next().is_none());
        assert!(!single.poll(single.phase_start + Duration::from_secs(5)));
    }

    #[test]
    fn fade_ramps_in_and_out() {
        let fade = Duration::from_secs(1);
        assert_eq!(phase_opacity(Phase::In, Duration::ZERO, fade), 0.0);
        assert!((phase_opacity(Phase::In, Duration::from_millis(500), fade) - 0.5).abs() < 0.01);
        assert!((phase_opacity(Phase::Out, Duration::from_millis(500), fade) - 0.5).abs() < 0.01);
        assert_eq!(phase_opacity(Phase::Hold, Duration::from_millis(500), fade), 1.0);
        assert_eq!(phase_opacity(Phase::In, Duration::from_secs(1), Duration::ZERO), 1.0);
    }

    #[test]
    fn config_round_trip_keeps_the_image_list() {
        let config = Config {
            sensors: SensorConfig {
                cpu: Some("k10temp:Tctl".to_string()),
                gpu: Some("amdgpu:edge".to_string()),
            },
            overlay: OverlayConfig {
                font: None,
                color: Some("f2f2f2".to_string()),
            },
            slideshow: SlideshowConfig {
                duration: 12.0,
                fade: 1.5,
                order: PlayOrder::Random,
            },
            images: vec![
                ImageConfig {
                    gif_path: "one.gif".to_string(),
                    boxes: false,
                    box_opacity: 150,
                    position: 0,
                },
                ImageConfig {
                    gif_path: "two.gif".to_string(),
                    boxes: true,
                    box_opacity: 80,
                    position: 100,
                },
            ],
        };
        let text = serde_yaml::to_string(&config).unwrap();
        assert!(text.contains("gifPath"));
        assert!(text.contains("boxOpacity"));
        let parsed = load_config_text(&text).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn a_short_image_entry_uses_defaults() {
        let config = load_config_text("images:\n- gifPath: one.gif\n").unwrap();
        assert!(!config.images[0].boxes);
        assert_eq!(config.images[0].box_opacity, 150);
        assert_eq!(config.images[0].position, 50);
        assert_eq!(config.slideshow.order, PlayOrder::Sequential);
        assert_eq!(config.slideshow.duration, 120.0);
        assert_eq!(config.slideshow.fade, 0.3);
        let text = serde_yaml::to_string(&config.slideshow).unwrap();
        assert!(text.contains("duration: 120"), "{text}");
        assert!(text.contains("fade: 0.3\n") || text.contains("fade: 0.3\r\n"), "{text}");
    }
}

#[cfg(test)]
mod rotation {
    use super::*;

    #[test]
    fn frames_rotate_clockwise_with_the_panel() {
        let mut image = RgbImage::from_pixel(2, 2, Rgb([0, 0, 0]));
        image.put_pixel(0, 0, Rgb([1, 0, 0]));
        assert_eq!(*rotate_frame(image.clone(), 0).get_pixel(0, 0), Rgb([1, 0, 0]));
        assert_eq!(*rotate_frame(image.clone(), 90).get_pixel(1, 0), Rgb([1, 0, 0]));
        assert_eq!(*rotate_frame(image.clone(), 180).get_pixel(1, 1), Rgb([1, 0, 0]));
        assert_eq!(*rotate_frame(image, 270).get_pixel(0, 1), Rgb([1, 0, 0]));
    }

    #[test]
    fn rotation_accepts_quarter_turns() {
        assert_eq!(parse_rotation("0").unwrap(), 0);
        assert_eq!(parse_rotation("90").unwrap(), 90);
        assert_eq!(parse_rotation("180").unwrap(), 180);
        assert_eq!(parse_rotation("270").unwrap(), 270);
        assert!(parse_rotation("45").is_err());
    }

    #[test]
    fn rotation_commands_reject_other_switches() {
        let mut query = Args::default();
        query.get_rotation = true;
        assert!(matches!(rotation_request(&query).unwrap(), Some(RotationCmd::Show)));

        let mut set = Args::default();
        set.set_rotation = Some(90);
        assert!(matches!(rotation_request(&set).unwrap(), Some(RotationCmd::Set(90))));

        query.debug = true;
        assert!(rotation_request(&query).is_err());
        set.gif = Some(PathBuf::from("a.gif"));
        assert!(rotation_request(&set).is_err());

        let mut both = Args::default();
        both.get_rotation = true;
        both.set_rotation = Some(180);
        assert!(rotation_request(&both).is_err());
        assert!(rotation_request(&Args::default()).unwrap().is_none());
    }
}

#[cfg(test)]
mod suite {
    use super::*;

    fn words(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    fn sensor(id: &str, chip: &str, label: &str, path: &str, fan: bool) -> Sensor {
        Sensor {
            id: id.to_string(),
            chip: chip.to_string(),
            label: label.to_string(),
            path: PathBuf::from(path),
            has_fan: fan,
            celsius: 40.0,
        }
    }

    #[test]
    fn switches_parse_and_rotation_stays_alone() {
        let args = parse_args_from(words(&["--gif", "demo.gif", "--debug"])).unwrap();
        assert!(args.debug);
        assert_eq!(args.gif.unwrap(), PathBuf::from("demo.gif"));

        let query = parse_args_from(words(&["--get-rotation"])).unwrap();
        assert!(matches!(rotation_request(&query).unwrap(), Some(RotationCmd::Show)));

        let mixed = parse_args_from(words(&["--get-rotation", "--debug"])).unwrap();
        assert!(rotation_request(&mixed).unwrap_err().to_string().contains("on its own"));
        assert!(parse_args_from(words(&["--set-rotation"])).is_err());
        assert!(parse_args_from(words(&["--nope"])).is_err());
    }

    #[test]
    fn deleting_an_image_matches_the_file_name() {
        let mut config = Config::default();
        config.images.push(ImageConfig {
            gif_path: "one.gif".to_string(),
            boxes: false,
            box_opacity: 150,
            position: 50,
        });
        config.images.push(ImageConfig {
            gif_path: "two.gif".to_string(),
            boxes: false,
            box_opacity: 150,
            position: 50,
        });
        assert!(delete_image(&mut config, None).is_err());
        delete_image(&mut config, Some(Path::new("/tmp/two.gif"))).unwrap();
        assert_eq!(config.images.len(), 1);
        assert_eq!(config.images[0].gif_path, "one.gif");
        delete_image(&mut config, None).unwrap();
        assert!(config.images.is_empty());
    }

    #[test]
    fn cpu_and_gpu_selection_prefers_the_labelled_chip() {
        let sensors = vec![
            sensor("k10temp:Tdie", "k10temp", "Tdie", "/sys/tdie", false),
            sensor("k10temp:Tctl", "k10temp", "Tctl", "/sys/tctl", false),
            sensor("amdgpu:junction", "amdgpu", "junction", "/sys/temp2_input", true),
            sensor("amdgpu:edge", "amdgpu", "edge", "/sys/temp1_input", true),
            sensor("nvme:Composite", "nvme", "Composite", "/sys/nvme", false),
        ];
        assert_eq!(pick_cpu(&sensors).unwrap().label, "Tctl");
        assert_eq!(pick_gpu(&sensors).unwrap().label, "edge");
        assert_eq!(resolve_sensor(&sensors, "k10temp:Tctl").unwrap().label, "Tctl");
        assert!(resolve_sensor(&sensors, "missing").is_err());
    }

    #[test]
    fn device_addresses_are_recognised() {
        assert!(is_pci_address("0000:03:00.0"));
        assert!(!is_pci_address("hwmon0"));
        assert!(is_i2c_address("0000-0018"));
        assert!(!is_i2c_address("card0"));
    }

    #[test]
    fn a_loaded_gif_reports_its_frames_and_size() {
        let image = RgbImage::from_pixel(2, 2, Rgb([1, 2, 3]));
        let slide = Slide {
            frames: vec![(image, Duration::from_millis(1))],
            boxes: false,
            opacity: 0,
        };
        assert_eq!(slide_bytes(&slide), 12);
        assert_eq!(load_summary(2, 2 * 1024 * 1024, None), "2 frames, 2.0 MB decoded");
        assert_eq!(
            load_summary(1, 512 * 1024, Some(3 * 1024 * 1024)),
            "1 frame, 0.5 MB decoded, 3.0 MB resident"
        );
    }
}
