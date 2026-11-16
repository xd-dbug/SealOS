use bootloader_api::info::{FrameBuffer, FrameBufferInfo, PixelFormat};
use conquer_once::spin::OnceCell;
use core::fmt;
use noto_sans_mono_bitmap::{get_raster, get_raster_width, FontWeight, RasterHeight};
use spin::Mutex;

const FONT_WEIGHT: FontWeight = FontWeight::Regular;
const CHAR_RASTER_HEIGHT: RasterHeight = RasterHeight::Size16;
const CHAR_HEIGHT: usize = CHAR_RASTER_HEIGHT.val();
const CHAR_WIDTH: usize = get_raster_width(FONT_WEIGHT, CHAR_RASTER_HEIGHT);

static WRITER: OnceCell<Mutex<Writer>> = OnceCell::uninit();

/// Software text console over the pixel framebuffer. Cursor position is in
/// pixels, not character cells, since glyph size comes from the font constants
/// above rather than a fixed grid.
pub struct Writer {
    buffer: &'static mut [u8],
    info: FrameBufferInfo,
    x: usize,
    y: usize,
}

impl Writer {
    /// Writes one grayscale pixel, translating `intensity` into the buffer's
    /// actual pixel format (RGB/BGR replicate it across channels; out-of-bounds is a no-op).
    fn write_pixel(&mut self, x: usize, y: usize, intensity: u8) {
        if x >= self.info.width || y >= self.info.height {
            return;
        }
        let byte_offset = (y * self.info.stride + x) * self.info.bytes_per_pixel;
        let pixel_bytes = &mut self.buffer[byte_offset..byte_offset + self.info.bytes_per_pixel];
        match self.info.pixel_format {
            PixelFormat::Rgb | PixelFormat::Bgr => {
                pixel_bytes[0] = intensity;
                pixel_bytes[1] = intensity;
                pixel_bytes[2] = intensity;
            }
            PixelFormat::U8 => {
                pixel_bytes[0] = intensity;
            }
            _ => {
                pixel_bytes[0] = intensity;
            }
        }
    }

    /// Blanks the whole framebuffer and resets the cursor to the top.
    fn clear(&mut self) {
        self.buffer.fill(0);
        self.y = 0;
    }

    /// Advances to the next line, scrolling by clearing the screen once
    /// there's no room left for another line (there's no real scroll buffer).
    fn newline(&mut self) {
        self.x = 0;
        if self.y + CHAR_HEIGHT * 2 > self.info.height {
            self.clear();
        } else {
            self.y += CHAR_HEIGHT;
        }
    }

    /// Moves the cursor back one glyph without drawing, like a terminal's
    /// BS: erasing is the writer's job (`"\x08 \x08"`: back, blank, back).
    ///
    /// At column 0 it stays put, as a VT100 does. Wrapping to the end of the
    /// previous line would need to know where that line's text ended, which
    /// isn't recorded, and would let a program back up into output above its
    /// own line. The cost: a typed line long enough to wrap can't be erased
    /// back across the wrap.
    fn backspace(&mut self) {
        self.x = self.x.saturating_sub(CHAR_WIDTH);
    }

    /// Cursor position in character cells, `(column, row)`.
    fn cursor(&self) -> (usize, usize) {
        (self.x / CHAR_WIDTH, self.y / CHAR_HEIGHT)
    }

    /// Renders one character's glyph at the cursor, wrapping/advancing as needed.
    pub fn write_char(&mut self, c: char) {
        match c {
            '\n' => self.newline(),
            '\r' => {}
            '\x08' => self.backspace(),
            c => {
                if self.x + CHAR_WIDTH > self.info.width {
                    self.newline();
                }
                let glyph = get_raster(c, FONT_WEIGHT, CHAR_RASTER_HEIGHT)
                    .unwrap_or_else(|| get_raster('?', FONT_WEIGHT, CHAR_RASTER_HEIGHT).unwrap());
                for (row_i, row) in glyph.raster().iter().enumerate() {
                    for (col_i, intensity) in row.iter().enumerate() {
                        self.write_pixel(self.x + col_i, self.y + row_i, *intensity);
                    }
                }
                self.x += CHAR_WIDTH;
            }
        }
    }
}

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            self.write_char(c);
        }
        Ok(())
    }
}

/// Sets up the global `WRITER` from the bootloader-provided framebuffer.
/// Must be called exactly once, before any `print!`/`println!`.
pub fn init(framebuffer: &'static mut FrameBuffer) {
    let info = framebuffer.info();
    let buffer = framebuffer.buffer_mut();
    let mut writer = Writer {
        buffer,
        info,
        x: 0,
        y: 0,
    };
    writer.clear();
    WRITER.init_once(|| Mutex::new(writer));
}

/// Backing function for the `print!`/`println!` macros. Silently drops
/// output if `init` hasn't run yet, since there's nowhere to write it.
///
/// Runs with interrupts disabled: exception handlers (`#BP`, `#PF`, `#GP`)
/// call `print!` too, and since `WRITER`'s spinlock isn't reentrant, one
/// arriving while this function already holds the lock would deadlock the
/// handler against the code it interrupted. The keyboard ISR used to echo
/// through here as well; it no longer prints, but the rule stays for the
/// handlers that do. `SYS_WRITE` (syscall context, IF=1) comes through this
/// same function, so it inherits the guard.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use fmt::Write;
    use x86_64::instructions::interrupts::without_interrupts;

    if let Some(writer) = WRITER.get() {
        without_interrupts(|| {
            writer.lock().write_fmt(args).ok();
        });
    }
}

/// Where the next glyph will be drawn, as `(column, row)` in character cells;
/// `None` before `init`. Exists so tests can check cursor movement (text,
/// newline, backspace) without comparing pixels.
pub fn cursor() -> Option<(usize, usize)> {
    use x86_64::instructions::interrupts::without_interrupts;

    WRITER.get().map(|writer| without_interrupts(|| writer.lock().cursor()))
}

/// Character cells per line, for the same tests; `None` before `init`.
pub fn columns() -> Option<usize> {
    use x86_64::instructions::interrupts::without_interrupts;

    WRITER.get().map(|writer| without_interrupts(|| writer.lock().info.width / CHAR_WIDTH))
}

/// Prints to the framebuffer console; a no-op until `framebuffer::init` runs.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::framebuffer::_print(format_args!($($arg)*)));
}

/// Like `print!`, appending a newline.
#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

#[test_case]
fn test_println_simple() {
    println!("test_println_simple output");
}

#[test_case]
fn test_println_many() {
    for _ in 0..200 {
        println!("test_println_many output");
    }
}