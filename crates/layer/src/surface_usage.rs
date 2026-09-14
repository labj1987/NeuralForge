//! Conservative admission for host capture. Never retry swapchain creation:
//! Vulkan retires oldSwapchain even when creation fails.
use ash::vk;

pub fn candidate(info: &vk::SwapchainCreateInfoKHR) -> bool {
    // Extended creation semantics (including usage-flags2 overrides, protected,
    // shared-present and stereo images) need separate validation before capture.
    info.p_next.is_null()
        && info.flags.is_empty()
        && info.image_array_layers == 1
        && matches!(info.present_mode, vk::PresentModeKHR::FIFO | vk::PresentModeKHR::FIFO_RELAXED
            | vk::PresentModeKHR::IMMEDIATE | vk::PresentModeKHR::MAILBOX)
}

pub fn requested_usage(original: vk::ImageUsageFlags, supported: vk::ImageUsageFlags) -> Option<vk::ImageUsageFlags> {
    let required = original | vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST;
    supported.contains(required).then_some(required)
}

/// Returns a private copy; the caller's create info and extension chain are never modified.
/// Unsupported or unqueryable surfaces use the original creation path without capture.
pub fn prepare(instance: &ash::Instance, query: vk::PFN_vkGetPhysicalDeviceSurfaceCapabilitiesKHR, physical: vk::PhysicalDevice, info: &vk::SwapchainCreateInfoKHR)
    -> Option<vk::SwapchainCreateInfoKHR>
{
    if !candidate(info) { return None; }
    // SAFETY: query was resolved through the same next-layer instance dispatch
    // chain as physical. Never send that handle through the loader trampoline.
    let mut caps = vk::SurfaceCapabilitiesKHR::default();
    unsafe { query(physical, info.surface, &mut caps) }.result().ok()?;
    let usage = requested_usage(info.image_usage, caps.supported_usage_flags)?;
    // Surface usage alone does not guarantee this format's implied image creation
    // parameters support the enlarged usage combination (VUID-imageFormat-01778).
    let props = unsafe { instance.get_physical_device_image_format_properties(
        physical, info.image_format, vk::ImageType::TYPE_2D, vk::ImageTiling::OPTIMAL,
        usage, vk::ImageCreateFlags::empty(),
    ) }.ok()?;
    if info.image_extent.width > props.max_extent.width
        || info.image_extent.height > props.max_extent.height
        || props.max_array_layers < 1 || !props.sample_counts.contains(vk::SampleCountFlags::TYPE_1)
    { return None; }
    let mut adjusted = *info;
    adjusted.image_usage = usage;
    Some(adjusted)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transfer_admission_preserves_application_usage() {
        let original = vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED;
        let supported = original | vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST;
        assert_eq!(requested_usage(original, supported), Some(supported));
        assert_eq!(requested_usage(original, supported & !vk::ImageUsageFlags::TRANSFER_SRC), None);
        assert_eq!(requested_usage(original, supported & !vk::ImageUsageFlags::TRANSFER_DST), None);
        assert_eq!(requested_usage(original, supported & !vk::ImageUsageFlags::SAMPLED), None);
    }
    #[test]
    fn extended_swapchains_are_not_captured() {
        let mut info = vk::SwapchainCreateInfoKHR::builder().image_array_layers(1).present_mode(vk::PresentModeKHR::FIFO).build();
        assert!(candidate(&info));
        info.present_mode = vk::PresentModeKHR::SHARED_DEMAND_REFRESH;
        assert!(!candidate(&info));
        info.present_mode = vk::PresentModeKHR::FIFO;
        info.image_array_layers = 2;
        assert!(!candidate(&info));
        info.image_array_layers = 1;
        info.flags = vk::SwapchainCreateFlagsKHR::PROTECTED;
        assert!(!candidate(&info));
        info.flags = vk::SwapchainCreateFlagsKHR::empty();
        let extension = vk::BaseInStructure::default();
        info.p_next = std::ptr::from_ref(&extension).cast();
        assert!(!candidate(&info));
    }
}
