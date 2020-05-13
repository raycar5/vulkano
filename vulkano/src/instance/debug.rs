// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

//! Debug callback called by intermediate layers or by the driver.
//!
//! When working on an application, it is recommended to register a debug callback. For example if
//! you enable the validation layers provided by the official Vulkan SDK, they will warn you about
//! invalid API usages or performance problems by calling this callback. The callback can also
//! be called by the driver or by whatever intermediate layer is activated.
//!
//! Note that the vulkano library can also emit messages to warn you about performance issues.
//! TODO: ^ that's not the case yet, need to choose whether we keep this idea
//!
//! # Example
//!
//! ```
//! # use vulkano::instance::Instance;
//! # use std::sync::Arc;
//! # let instance: Arc<Instance> = return;
//! use vulkano::instance::debug::DebugCallback;
//!
//! let _callback = DebugCallback::errors_and_warnings(&instance, |msg| {
//!     println!("Debug callback: {:?}", msg.description);
//! }).ok();
//! ```
//!
//! The type of `msg` in the callback is [`Message`](struct.Message.html).
//!
//! Note that you must keep the `_callback` object alive for as long as you want your callback to
//! be callable. If you don't store the return value of `DebugCallback`'s constructor in a
//! variable, it will be immediately destroyed and your callback will not work.
//!

use std::error;
use std::ffi::CStr;
use std::fmt;
use std::mem::MaybeUninit;
use std::os::raw::c_void;
use std::panic;
use std::ptr;
use std::sync::Arc;

use instance::Instance;

use check_errors;
use vk;
use vk::{Bool32, DebugUtilsMessengerCallbackDataEXT};
use Error;
use VulkanObject;

/// Registration of a callback called by validation layers.
///
/// The callback can be called as long as this object is alive.
#[must_use = "The DebugCallback object must be kept alive for as long as you want your callback \
              to be called"]
pub struct DebugCallback {
    instance: Arc<Instance>,
    debug_report_callback: vk::DebugUtilsMessengerEXT,
    user_callback: Box<Box<dyn Fn(&Message) + Send>>,
}

impl DebugCallback {
    /// Initializes a debug callback.
    ///
    /// Panics generated by calling `user_callback` are ignored.
    pub fn new<F>(
        instance: &Arc<Instance>,
        severity: MessageSeverity,
        ty: MessageType,
        user_callback: F,
    ) -> Result<DebugCallback, DebugCallbackCreationError>
    where
        F: Fn(&Message) + 'static + Send + panic::RefUnwindSafe,
    {
        if !instance.loaded_extensions().ext_debug_utils {
            return Err(DebugCallbackCreationError::MissingExtension);
        }

        // Note that we need to double-box the callback, because a `*const Fn()` is a fat pointer
        // that can't be cast to a `*const c_void`.
        let user_callback = Box::new(Box::new(user_callback) as Box<_>);

        extern "system" fn callback(
            severity: vk::DebugUtilsMessageSeverityFlagsEXT,
            ty: vk::DebugUtilsMessageTypeFlagsEXT,
            callback_data: *const DebugUtilsMessengerCallbackDataEXT,
            user_data: *mut c_void,
        ) -> Bool32 {
            unsafe {
                let user_callback = user_data as *mut Box<dyn Fn()> as *const _;
                let user_callback: &Box<dyn Fn(&Message)> = &*user_callback;

                let layer_prefix = CStr::from_ptr((*callback_data).pMessageIdName)
                    .to_str()
                    .expect("debug callback message not utf-8");
                let description = CStr::from_ptr((*callback_data).pMessage)
                    .to_str()
                    .expect("debug callback message not utf-8");

                let message = Message {
                    severity: MessageSeverity {
                        information: (severity & vk::DEBUG_UTILS_MESSAGE_SEVERITY_INFO_BIT_EXT)
                            != 0,
                        warning: (severity & vk::DEBUG_UTILS_MESSAGE_SEVERITY_WARNING_BIT_EXT) != 0,
                        error: (severity & vk::DEBUG_UTILS_MESSAGE_SEVERITY_ERROR_BIT_EXT) != 0,
                        verbose: (severity & vk::DEBUG_UTILS_MESSAGE_SEVERITY_VERBOSE_BIT_EXT) != 0,
                    },
                    ty: MessageType {
                        general: (ty & vk::DEBUG_UTILS_MESSAGE_TYPE_GENERAL_BIT_EXT) != 0,
                        validation: (ty & vk::DEBUG_UTILS_MESSAGE_TYPE_VALIDATION_BIT_EXT) != 0,
                        performance: (ty & vk::DEBUG_UTILS_MESSAGE_TYPE_PERFORMANCE_BIT_EXT) != 0,
                    },
                    layer_prefix,
                    description,
                };

                // Since we box the closure, the type system doesn't detect that the `UnwindSafe`
                // bound is enforced. Therefore we enforce it manually.
                let _ = panic::catch_unwind(panic::AssertUnwindSafe(move || {
                    user_callback(&message);
                }));

                vk::FALSE
            }
        }

        let severity = {
            let mut flags = 0;
            if severity.information {
                flags |= vk::DEBUG_UTILS_MESSAGE_SEVERITY_INFO_BIT_EXT;
            }
            if severity.warning {
                flags |= vk::DEBUG_UTILS_MESSAGE_SEVERITY_WARNING_BIT_EXT;
            }
            if severity.error {
                flags |= vk::DEBUG_UTILS_MESSAGE_SEVERITY_ERROR_BIT_EXT;
            }
            if severity.verbose {
                flags |= vk::DEBUG_UTILS_MESSAGE_SEVERITY_VERBOSE_BIT_EXT;
            }
            flags
        };

        let ty = {
            let mut flags = 0;
            if ty.general {
                flags |= vk::DEBUG_UTILS_MESSAGE_TYPE_GENERAL_BIT_EXT;
            }
            if ty.validation {
                flags |= vk::DEBUG_UTILS_MESSAGE_TYPE_VALIDATION_BIT_EXT;
            }
            if ty.performance {
                flags |= vk::DEBUG_UTILS_MESSAGE_TYPE_PERFORMANCE_BIT_EXT;
            }
            flags
        };

        let infos = vk::DebugUtilsMessengerCreateInfoEXT {
            sType: vk::STRUCTURE_TYPE_DEBUG_UTILS_MESSENGER_CREATE_INFO_EXT,
            pNext: ptr::null(),
            flags: 0,
            messageSeverity: severity,
            messageType: ty,
            pfnUserCallback: callback,
            pUserData: &*user_callback as &Box<_> as *const Box<_> as *const c_void as *mut _,
        };

        let vk = instance.pointers();

        let debug_report_callback = unsafe {
            let mut output = MaybeUninit::uninit();
            check_errors(vk.CreateDebugUtilsMessengerEXT(
                instance.internal_object(),
                &infos,
                ptr::null(),
                output.as_mut_ptr(),
            ))?;
            output.assume_init()
        };

        Ok(DebugCallback {
            instance: instance.clone(),
            debug_report_callback,
            user_callback,
        })
    }

    /// Initializes a debug callback with errors and warnings.
    ///
    /// Shortcut for `new(instance, MessageTypes::errors_and_warnings(), user_callback)`.
    #[inline]
    pub fn errors_and_warnings<F>(
        instance: &Arc<Instance>,
        user_callback: F,
    ) -> Result<DebugCallback, DebugCallbackCreationError>
    where
        F: Fn(&Message) + Send + 'static + panic::RefUnwindSafe,
    {
        DebugCallback::new(
            instance,
            MessageSeverity::errors_and_warnings(),
            MessageType::general(),
            user_callback,
        )
    }
}

impl Drop for DebugCallback {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            let vk = self.instance.pointers();
            vk.DestroyDebugUtilsMessengerEXT(
                self.instance.internal_object(),
                self.debug_report_callback,
                ptr::null(),
            );
        }
    }
}

/// A message received by the callback.
pub struct Message<'a> {
    /// Severity of message.
    pub severity: MessageSeverity,
    /// Type of message,
    pub ty: MessageType,
    /// Prefix of the layer that reported this message.
    pub layer_prefix: &'a str,
    /// Description of the message.
    pub description: &'a str,
}

/// Severity of message.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct MessageSeverity {
    /// An error that may cause undefined results, including an application crash.
    pub error: bool,
    /// An unexpected use.
    pub warning: bool,
    /// An informational message that may be handy when debugging an application.
    pub information: bool,
    /// Diagnostic information from the loader and layers.
    pub verbose: bool,
}

impl MessageSeverity {
    /// Builds a `MessageSeverity` with all fields set to `false` expect `error`.
    #[inline]
    pub fn errors() -> MessageSeverity {
        MessageSeverity {
            error: true,
            ..MessageSeverity::none()
        }
    }

    /// Builds a `MessageSeverity` with all fields set to `false` expect `error`, `warning`
    /// and `performance_warning`.
    #[inline]
    pub fn errors_and_warnings() -> MessageSeverity {
        MessageSeverity {
            error: true,
            warning: true,
            ..MessageSeverity::none()
        }
    }

    /// Builds a `MessageSeverity` with all fields set to `false`.
    #[inline]
    pub fn none() -> MessageSeverity {
        MessageSeverity {
            error: false,
            warning: false,
            information: false,
            verbose: false,
        }
    }
}

/// Type of message.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct MessageType {
    /// Specifies that some general event has occurred.
    pub general: bool,
    /// Specifies that something has occurred during validation against the vulkan specification
    pub validation: bool,
    /// Specifies a potentially non-optimal use of Vulkan
    pub performance: bool,
}

impl MessageType {
    /// Builds a `MessageType` with general field set to `true`.
    #[inline]
    pub fn general() -> MessageType {
        MessageType {
            general: true,
            validation: false,
            performance: false,
        }
    }

    /// Builds a `MessageType` with all fields set to `true`.
    #[inline]
    pub fn all() -> MessageType {
        MessageType {
            general: true,
            validation: true,
            performance: true,
        }
    }

    /// Builds a `MessageType` with all fields set to `false`.
    #[inline]
    pub fn none() -> MessageType {
        MessageType {
            general: false,
            validation: false,
            performance: false,
        }
    }
}

/// Error that can happen when creating a debug callback.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DebugCallbackCreationError {
    /// The `EXT_debug_report` extension was not enabled.
    MissingExtension,
}

impl error::Error for DebugCallbackCreationError {}

impl fmt::Display for DebugCallbackCreationError {
    #[inline]
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(fmt, "{}", match *self {
            DebugCallbackCreationError::MissingExtension => {
                "the `EXT_debug_report` extension was not enabled"
            }
        })
    }
}

impl From<Error> for DebugCallbackCreationError {
    #[inline]
    fn from(err: Error) -> DebugCallbackCreationError {
        panic!("unexpected error: {:?}", err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    #[test]
    fn ensure_sendable() {
        // It's useful to be able to initialize a DebugCallback on one thread
        // and keep it alive on another thread.
        let instance = instance!();
        let severity = MessageSeverity::none();
        let ty = MessageType::all();
        let callback = DebugCallback::new(&instance, severity, ty, |_| {});
        thread::spawn(move || {
            let _ = callback;
        });
    }
}
