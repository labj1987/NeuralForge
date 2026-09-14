//! Intercept destruction before the framework destroys the downstream device.
//! All other dispatch and loader bookkeeping remains in the pinned framework.
use ash::vk;
use std::ffi::{c_char, CStr};
use vulkan_layer::Global;
type Framework = Global<crate::NeuralForgeLayer>;
pub struct EntryPoints;

impl EntryPoints {
    pub unsafe extern "system" fn enumerate_instance_layer_properties(count: *mut u32, props: *mut vk::LayerProperties) -> vk::Result {
        unsafe { Framework::enumerate_instance_layer_properties(count, props) }
    }
    pub unsafe extern "system" fn enumerate_instance_extension_properties(name: *const c_char, count: *mut u32, props: *mut vk::ExtensionProperties) -> vk::Result {
        unsafe { Framework::enumerate_instance_extension_properties(name, count, props) }
    }
    pub unsafe extern "system" fn enumerate_device_layer_properties(pd: vk::PhysicalDevice, count: *mut u32, props: *mut vk::LayerProperties) -> vk::Result {
        unsafe { Framework::enumerate_device_layer_properties(pd, count, props) }
    }
    pub unsafe extern "system" fn enumerate_device_extension_properties(pd: vk::PhysicalDevice, name: *const c_char, count: *mut u32, props: *mut vk::ExtensionProperties) -> vk::Result {
        unsafe { Framework::enumerate_device_extension_properties(pd, name, count, props) }
    }
    pub unsafe extern "system" fn get_instance_proc_addr(instance: vk::Instance, name: *const c_char) -> vk::PFN_vkVoidFunction {
        let original = unsafe { Framework::get_instance_proc_addr(instance, name) };
        unsafe { Self::intercept(name, original) }
    }
    pub unsafe extern "system" fn get_device_proc_addr(device: vk::Device, name: *const c_char) -> vk::PFN_vkVoidFunction {
        let original = unsafe { Framework::get_device_proc_addr(device, name) };
        unsafe { Self::intercept(name, original) }
    }
    unsafe fn intercept(name: *const c_char, original: vk::PFN_vkVoidFunction) -> vk::PFN_vkVoidFunction {
        // Preserve null results (including invalid instance/device queries). Keep
        // subsequent proc-address queries routed through this same interception.
        original?;
        match unsafe { CStr::from_ptr(name) }.to_bytes() {
            b"vkDestroyDevice" => Some(unsafe { std::mem::transmute::<vk::PFN_vkDestroyDevice, unsafe extern "system" fn()>(Self::destroy_device) }),
            b"vkGetInstanceProcAddr" => Some(unsafe { std::mem::transmute::<vk::PFN_vkGetInstanceProcAddr, unsafe extern "system" fn()>(Self::get_instance_proc_addr) }),
            b"vkGetDeviceProcAddr" => Some(unsafe { std::mem::transmute::<vk::PFN_vkGetDeviceProcAddr, unsafe extern "system" fn()>(Self::get_device_proc_addr) }),
            _ => original,
        }
    }
    unsafe extern "system" fn destroy_device(device: vk::Device, allocator: *const vk::AllocationCallbacks) {
        if device == vk::Device::null() { return; }
        // Ask the framework directly, bypassing this adapter. Its original function
        // must still remove device dispatch bookkeeping and forward the allocator.
        let original = unsafe { Framework::get_device_proc_addr(device, c"vkDestroyDevice".as_ptr()) };
        if let Some(original) = original {
            unsafe { crate::device::destroy_private_resources(device); }
            let destroy: vk::PFN_vkDestroyDevice = unsafe { std::mem::transmute(original) };
            unsafe { destroy(device, allocator); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "system" fn dummy() {}
    #[test]
    fn intercept_preserves_null_and_unrelated_commands() {
        unsafe {
            assert!(EntryPoints::intercept(c"vkDestroyDevice".as_ptr(), None).is_none());
            assert_eq!(EntryPoints::intercept(c"vkQueuePresentKHR".as_ptr(), Some(dummy)).unwrap() as usize, dummy as *const () as usize);
            for name in [c"vkDestroyDevice", c"vkGetInstanceProcAddr", c"vkGetDeviceProcAddr"] {
                assert_ne!(EntryPoints::intercept(name.as_ptr(), Some(dummy)).unwrap() as usize, dummy as *const () as usize);
            }
        }
    }
}
