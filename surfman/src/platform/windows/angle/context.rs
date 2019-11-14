// surfman/surfman/src/platform/windows/angle/context.rs
//
//! Wrapper for EGL contexts managed by ANGLE using Direct3D 11 as a backend on Windows.

use crate::context::{CREATE_CONTEXT_MUTEX, ContextID};
use crate::egl::types::{EGLConfig, EGLContext, EGLint};
use crate::egl;
use crate::gl::Gl;
use crate::platform::generic::egl::context::{self, CurrentContextGuard};
use crate::platform::generic::egl::device::EGL_FUNCTIONS;
use crate::platform::generic::egl::error::ToWindowingApiError;
use crate::surface::Framebuffer;
use crate::{ContextAttributes, Error, SurfaceInfo};
use super::device::Device;
use super::surface::{Surface, Win32Objects};

use std::mem;
use std::os::raw::c_void;
use std::thread;
use winapi::shared::winerror::S_OK;
use winapi::um::winbase::INFINITE;

pub use crate::platform::generic::egl::context::ContextDescriptor;

thread_local! {
    #[doc(hidden)]
    pub static GL_FUNCTIONS: Gl = Gl::load_with(context::get_proc_address);
}

/// Represents an OpenGL rendering context.
/// 
/// A context allows you to issue rendering commands to a surface. When initially created, a
/// context has no attached surface, so rendering commands will fail or be ignored. Typically, you
/// attach a surface to the context before rendering.
/// 
/// Contexts take ownership of the surfaces attached to them. In order to mutate a surface in any
/// way other than rendering to it (e.g. presenting it to a window, which causes a buffer swap), it
/// must first be detached from its context. Each surface is associated with a single context upon
/// creation and may not be rendered to from any other context. However, you can wrap a surface in
/// a surface texture, which allows the surface to be read from another context.
/// 
/// OpenGL objects may not be shared across contexts directly, but surface textures effectively
/// allow for sharing of texture data. Contexts are local to a single thread and device.
/// 
/// A context must be explicitly destroyed with `destroy_context()`, or a panic will occur.
pub struct Context {
    pub(crate) egl_context: EGLContext,
    pub(crate) id: ContextID,
    framebuffer: Framebuffer<Surface>,
    context_is_owned: bool,
}

impl Drop for Context {
    #[inline]
    fn drop(&mut self) {
        if self.egl_context != egl::NO_CONTEXT && !thread::panicking() {
            panic!("Contexts must be destroyed explicitly with `destroy_context`!")
        }
    }
}

impl Device {
    /// Creates a context descriptor with the given attributes.
    /// 
    /// Context descriptors are local to this device.
    #[inline]
    pub fn create_context_descriptor(&self, attributes: &ContextAttributes)
                                     -> Result<ContextDescriptor, Error> {
        unsafe {
            ContextDescriptor::new(self.egl_display, attributes, &[
                egl::BIND_TO_TEXTURE_RGBA as EGLint,    1 as EGLint,
                egl::SURFACE_TYPE as EGLint,            egl::PBUFFER_BIT as EGLint,
                egl::RENDERABLE_TYPE as EGLint,         egl::OPENGL_ES2_BIT as EGLint,
            ])
        }
    }

    /// Creates a new OpenGL context.
    /// 
    /// The context initially has no surface attached. Until a surface is bound to it, rendering
    /// commands will fail or have no effect.
    pub fn create_context(&mut self, descriptor: &ContextDescriptor) -> Result<Context, Error> {
        let mut next_context_id = CREATE_CONTEXT_MUTEX.lock().unwrap();
        unsafe {
            let egl_context = context::create_context(self.egl_display, descriptor)?;

            let context = Context {
                egl_context,
                id: *next_context_id,
                framebuffer: Framebuffer::None,
                context_is_owned: true,
            };
            next_context_id.0 += 1;
            Ok(context)
        }
    }

    /// Destroys a context.
    /// 
    /// The context must have been created on this device.
    pub fn destroy_context(&self, context: &mut Context) -> Result<(), Error> {
        if context.egl_context == egl::NO_CONTEXT {
            return Ok(());
        }

        if let Ok(Some(mut surface)) = self.unbind_surface_from_context(context) {
            self.destroy_surface(context, &mut surface)?;
        }

        EGL_FUNCTIONS.with(|egl| {
            unsafe {
                egl.MakeCurrent(self.egl_display,
                                egl::NO_SURFACE,
                                egl::NO_SURFACE,
                                egl::NO_CONTEXT);

                if context.context_is_owned {
                    let result = egl.DestroyContext(self.egl_display, context.egl_context);
                    egl.DestroyContext(self.egl_display, context.egl_context);
                    assert_ne!(result, egl::FALSE);
                }
            }
        });

        Ok(())
    }

    /// Returns the descriptor that this context was created with.
    pub fn context_descriptor(&self, context: &Context) -> ContextDescriptor {
        unsafe {
            ContextDescriptor::from_egl_context(self.egl_display, context.egl_context)
        }
    }

    /// Makes the context the current OpenGL context for this thread.
    /// 
    /// After calling this function, it is valid to use OpenGL rendering commands.
    pub fn make_context_current(&self, context: &Context) -> Result<(), Error> {
        unsafe {
            let egl_surface = match context.framebuffer {
                Framebuffer::Surface(ref surface) => surface.egl_surface,
                Framebuffer::None => egl::NO_SURFACE,
                Framebuffer::External => return Err(Error::ExternalRenderTarget),
            };

            EGL_FUNCTIONS.with(|egl| {
                let result = egl.MakeCurrent(self.egl_display,
                                             egl_surface,
                                             egl_surface,
                                             context.egl_context);
                if result == egl::FALSE {
                    let err = egl.GetError().to_windowing_api_error();
                    return Err(Error::MakeCurrentFailed(err));
                }
                Ok(())
            })
        }
    }

    /// Removes the current OpenGL context from this thread.
    /// 
    /// After calling this function, OpenGL rendering commands will fail until a new context is
    /// made current.
    pub fn make_no_context_current(&self) -> Result<(), Error> {
        unsafe {
            context::make_no_context_current(self.egl_display)
        }
    }

    fn temporarily_make_context_current(&self, context: &Context)
                                        -> Result<CurrentContextGuard, Error> {
        let guard = CurrentContextGuard::new();
        self.make_context_current(context)?;
        Ok(guard)
    }

    pub(crate) fn context_is_current(&self, context: &Context) -> bool {
        EGL_FUNCTIONS.with(|egl| {
            unsafe {
                egl.GetCurrentContext() == context.egl_context
            }
        })
    }

    /// Returns the attributes that the context descriptor was created with.
    #[inline]
    pub fn context_descriptor_attributes(&self, context_descriptor: &ContextDescriptor)
                                         -> ContextAttributes {
        unsafe {
            context_descriptor.attributes(self.egl_display)
        }
    }

    /// Fetches the address of an OpenGL function associated with this context.
    /// 
    /// OpenGL functions are local to a context. You should not use OpenGL functions on one context
    /// with any other context.
    /// 
    /// This method is typically used with a function like `gl::load_with()` from the `gl` crate to
    /// load OpenGL function pointers.
    #[inline]
    pub fn get_proc_address(&self, _: &Context, symbol_name: &str) -> *const c_void {
        context::get_proc_address(symbol_name)
    }

    #[inline]
    pub(crate) fn context_descriptor_to_egl_config(&self, context_descriptor: &ContextDescriptor)
                                                   -> EGLConfig {
        unsafe {
            context::egl_config_from_id(self.egl_display,
                                        context_descriptor.egl_config_id)
        }
    }

    /// Attaches a surface to a context for rendering.
    /// 
    /// This function takes ownership of the surface. The surface must have been created with this
    /// context, or an `IncompatibleSurface` error is returned.
    /// 
    /// If this function is called with a surface already bound, a `SurfaceAlreadyBound` error is
    /// returned. To avoid this error, first unbind the existing surface with
    /// `unbind_surface_from_context`.
    /// 
    /// If an error is returned, the surface is returned alongside it.
    pub fn bind_surface_to_context(&self, context: &mut Context, surface: Surface)
                                   -> Result<(), (Error, Surface)> {
        if context.id != surface.context_id {
            return Err((Error::IncompatibleSurface, surface));
        }

        match context.framebuffer {
            Framebuffer::None => {}
            Framebuffer::External => return Err((Error::ExternalRenderTarget, surface)),
            Framebuffer::Surface(_) => return Err((Error::SurfaceAlreadyBound, surface)),
        }

        // If the surface does not use a DXGI keyed mutex, then finish.
        // FIXME(pcwalton): Is this necessary and sufficient?
        if !surface.uses_keyed_mutex() {
            if let Ok(_guard) = self.temporarily_make_context_current(context) {
                unsafe {
                    GL_FUNCTIONS.with(|gl| gl.Finish());
                }
            }
        }

        let is_current = self.context_is_current(context);

        match surface.win32_objects {
            Win32Objects::Pbuffer { keyed_mutex: Some(ref keyed_mutex), .. } => {
                unsafe {
                    let result = keyed_mutex.AcquireSync(0, INFINITE);
                    assert_eq!(result, S_OK);
                }
            }
            _ => {}
        }

        context.framebuffer = Framebuffer::Surface(surface);

        if is_current {
            // We need to make ourselves current again, because the surface changed.
            drop(self.make_context_current(context));
        }

        Ok(())
    }

    /// Removes and returns any attached surface from this context.
    /// 
    /// Any pending OpenGL commands targeting this surface will be automatically flushed, so the
    /// surface is safe to read from immediately when this function returns.
    pub fn unbind_surface_from_context(&self, context: &mut Context)
                                       -> Result<Option<Surface>, Error> {
        match context.framebuffer {
            Framebuffer::None => return Ok(None),
            Framebuffer::External => return Err(Error::ExternalRenderTarget),
            Framebuffer::Surface(_) => {}
        }

        let surface = match mem::replace(&mut context.framebuffer, Framebuffer::None) {
            Framebuffer::Surface(surface) => surface,
            Framebuffer::None | Framebuffer::External => unreachable!(),
        };

        match surface.win32_objects {
            Win32Objects::Pbuffer { keyed_mutex: Some(ref keyed_mutex), .. } => {
                unsafe {
                    let result = keyed_mutex.ReleaseSync(0);
                    assert_eq!(result, S_OK);
                }
            }
            _ => {}
        }

        Ok(Some(surface))
    }

    /// Returns a unique ID representing a context.
    /// 
    /// This ID is unique to all currently-allocated contexts. If you destroy a context and create
    /// a new one, the new context might have the same ID as the destroyed one.
    #[inline]
    pub fn context_id(&self, context: &Context) -> ContextID {
        context.id
    }

    /// Returns various information about the surface attached to a context.
    /// 
    /// This includes, most notably, the OpenGL framebuffer object needed to render to the surface.
    pub fn context_surface_info(&self, context: &Context) -> Result<Option<SurfaceInfo>, Error> {
        match context.framebuffer {
            Framebuffer::None => Ok(None),
            Framebuffer::External => Err(Error::ExternalRenderTarget),
            Framebuffer::Surface(ref surface) => Ok(Some(self.surface_info(surface))),
        }
    }
}
