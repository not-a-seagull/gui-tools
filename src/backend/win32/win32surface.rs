// MIT/Apache2 License

use crate::{
    color::{colors, Rgba},
    event::EventTypeMask,
    geometry::Rectangle,
    graphics::GraphicsInternal,
    mutex::ShimRwLock as RwLock,
    runtime::Runtime,
    surface::{SurfaceBackend, SurfaceInitialization},
};
use core::{
    convert::TryInto,
    mem::{self, MaybeUninit},
    ptr::{self, NonNull},
};
use cty::c_int;
use winapi::{
    ctypes::c_void,
    shared::{
        minwindef::{FALSE, TRUE},
        ntdef::CHAR,
        windef::{HBRUSH__, HDC__, HPEN__, HWND, HWND__, RECT},
    },
    um::{wingdi, winuser},
};

struct BackgroundDetails {
    // details for drawing the background
    background_brush: Option<NonNull<HBRUSH__>>,
    // details for drawing the border
    border_width: u32,
    border_color: Rgba,
    border_pen: Option<NonNull<HPEN__>>,
}

impl Default for BackgroundDetails {
    #[inline]
    fn default() -> Self {
        Self {
            background_brush: None,
            border_width: 0,
            border_color: colors::BLACK,
            border_pen: None,
        }
    }
}

pub struct Win32Surface {
    hwnd: NonNull<HWND__>,
    // if we are drawing, this is the current DC
    current_dc: Option<NonNull<HDC__>>,

    // details for drawing the background
    background_details: RwLock<BackgroundDetails>,
}

impl Win32Surface {
    #[inline]
    pub fn new(runtime: &Runtime, init: &SurfaceInitialization) -> crate::Result<Self> {
        // create the window class if it hasn't been created already
        let wruntime = runtime.as_win32().unwrap();
        let class_name = wruntime.create_window_class()?;

        // figure out the starting location
        let (parent_width, parent_height) = match init.parent {
            None => {
                // just use the monitor width and height
                runtime.default_monitor().unwrap().size()
            }
            Some(hwnd_num) => {
                // get the window rect and calculate from that
                let mut rect: MaybeUninit<RECT> = MaybeUninit::zeroed();
                if unsafe {
                    winuser::GetWindowRect(
                        mem::transmute::<usize, HWND>(hwnd_num),
                        rect.as_mut_ptr(),
                    )
                } == 0
                {
                    // if it failed, just use monitor width and height
                    runtime.default_monitor().unwrap().size()
                } else {
                    let rect = unsafe { rect.assume_init() };
                    let hw = (rect.right - rect.left, rect.bottom - rect.top);
                    (hw.0.try_into()?, hw.1.try_into()?)
                }
            }
        };

        let (x, y) =
            init.starting_point
                .to_x_y(init.width, init.height, parent_width, parent_height);

        // remove our current references to stuff
        mem::drop(wruntime);

        // create the window proper
        let hwnd = unsafe {
            winuser::CreateWindowExA(
                winuser::WS_EX_CLIENTEDGE,
                class_name.as_ref(),
                init.title.as_bytes().as_ptr() as *const CHAR,
                winuser::WS_OVERLAPPEDWINDOW,
                x,
                y,
                init.width as c_int,
                init.height as c_int,
                match init.parent {
                    Some(hwnd) => mem::transmute::<usize, HWND>(hwnd),
                    None => ptr::null_mut(),
                },
                ptr::null_mut(),
                ptr::null_mut(),
                runtime.clone().into_ptr() as *mut c_void,
            )
        };

        let hwnd = match NonNull::new(hwnd) {
            Some(hwnd) => hwnd,
            None => return Err(crate::win32error("CreateWindowExA")),
        };

        // show the window
        unsafe { winuser::ShowWindow(hwnd.as_ptr(), winuser::SW_SHOW) };

        Ok(Self {
            hwnd,
            current_dc: None,
            background_details: RwLock::new(Default::default()),
        })
    }

    /// Helper to set the border details.
    #[inline]
    fn set_border_details<F>(&self, setter: F) -> crate::Result<()>
    where
        F: FnOnce(&mut BackgroundDetails),
    {
        // delete a pen if it already exists
        let mut background_details = self.background_details.write();
        if let Some(pen) = background_details.border_pen.take() {
            unsafe { wingdi::DeleteObject(pen.as_ptr() as *mut _) };
        }

        // set details using the function
        setter(&mut background_details);

        // create a new pen from the details
        let (r, g, b, _) = background_details.border_color.convert_elements::<u8>();
        let pen = unsafe {
            wingdi::CreatePen(
                wingdi::PS_SOLID as _,
                background_details.border_width.try_into()?,
                wingdi::RGB(r, g, b),
            )
        };
        let pen = match NonNull::new(pen) {
            Some(pen) => pen,
            None => return Err(crate::win32error("CreatePen")),
        };

        background_details.border_pen = Some(pen);
        mem::drop(background_details);

        self.invalidate(None)?;
        Ok(())
    }
}

impl SurfaceBackend for Win32Surface {
    #[inline]
    fn id(&self) -> usize {
        self.hwnd.as_ptr() as *const () as usize
    }

    #[inline]
    fn set_event_mask(&self, _mask: &[EventTypeMask]) -> crate::Result<()> {
        Ok(())
    }

    #[inline]
    fn set_location(&self, x: i32, y: i32) -> crate::Result<()> {
        if unsafe {
            winuser::SetWindowPos(
                self.hwnd.as_ptr(),
                winuser::HWND_TOP,
                x,
                y,
                0,
                0,
                winuser::SWP_NOSIZE,
            )
        } == 0
        {
            Err(crate::win32error("SetWindowPos"))
        } else {
            Ok(())
        }
    }

    #[inline]
    fn set_size(&self, width: u32, height: u32) -> crate::Result<()> {
        if unsafe {
            winuser::SetWindowPos(
                self.hwnd.as_ptr(),
                winuser::HWND_TOP,
                0,
                0,
                width as c_int,
                height as c_int,
                winuser::SWP_NOMOVE,
            )
        } == 0
        {
            Err(crate::win32error("SetWindowPos"))
        } else {
            Ok(())
        }
    }

    #[inline]
    fn set_background_color(&self, clr: Rgba) -> crate::Result<()> {
        // if there is a current background brush, delete it
        let mut background_details = self.background_details.write();
        if let Some(brush) = background_details.background_brush.take() {
            unsafe { wingdi::DeleteObject(brush.as_ptr() as *mut _) };
        }

        // create a simple brush
        let (r, g, b, _) = clr.convert_elements::<u8>();
        let brush = unsafe { wingdi::CreateSolidBrush(wingdi::RGB(r, g, b)) };
        let brush = match NonNull::new(brush) {
            Some(brush) => brush,
            None => return Err(crate::win32error("CreateSolidBrush")),
        };

        // set the brush in the details
        background_details.background_brush = Some(brush);
        mem::drop(background_details);

        // invalidate the window for a repaint
        self.invalidate(None)
    }

    #[inline]
    fn set_border_color(&self, clr: Rgba) -> crate::Result<()> {
        self.set_border_details(|details| {
            details.border_color = clr;
        })
    }

    #[inline]
    fn set_border_width(&self, width: u32) -> crate::Result<()> {
        self.set_border_details(|details| {
            details.border_width = width;
        })
    }

    #[inline]
    fn invalidate(&self, rectangle: Option<Rectangle>) -> crate::Result<()> {
        let bounds_rect: Option<RECT> = rectangle.map(|r| RECT {
            left: r.x() as _,
            top: r.y() as _,
            right: (r.x() + r.width() as i32) as _,
            bottom: (r.y() + r.height() as i32) as _,
        });

        if unsafe {
            winuser::InvalidateRect(
                self.hwnd.as_ptr(),
                match bounds_rect {
                    Some(ref br) => br,
                    None => ptr::null(),
                },
                TRUE,
            )
        } == 0
        {
            Err(crate::win32error("InvalidateRect"))
        } else {
            Ok(())
        }
    }

    #[inline]
    fn graphics_internal(&self) -> crate::Result<NonNull<dyn GraphicsInternal>> {
        unimplemented!()
    }
}
