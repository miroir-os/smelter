use std::ptr::NonNull;

use ash::vk;

use crate::{VulkanDecoderError, parameters::H265Profile};

pub(crate) struct VkH265VideoParameterSet {
    pub(crate) vps: vk::native::StdVideoH265VideoParameterSet,

    profile_tier_level: Option<NonNull<[vk::native::StdVideoH265ProfileTierLevel]>>,
    dec_pic_buf_mgr: Option<NonNull<vk::native::StdVideoH265DecPicBufMgr>>,
}

impl VkH265VideoParameterSet {
    fn new_encode(profile: H265Profile, max_references: u32) -> Self {
        let profile_tier_level = vec![vk::native::StdVideoH265ProfileTierLevel {
            flags: vk::native::StdVideoH265ProfileTierLevelFlags {
                _bitfield_align_1: [],
                _bitfield_1: vk::native::StdVideoH265ProfileTierLevelFlags::new_bitfield_1(
                    1, 1, 0, 1, 1,
                ),
                __bindgen_padding_0: [0; 3],
            },
            general_profile_idc: profile.to_profile_idc(),
            general_level_idc: vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_1,
        }]
        .into_boxed_slice();
        let profile_tier_level = NonNull::from(Box::leak(profile_tier_level));

        let mut dec_pic_buf_mgr = Box::new(vk::native::StdVideoH265DecPicBufMgr {
            max_num_reorder_pics: [0; 7],
            max_dec_pic_buffering_minus1: [0; 7],
            max_latency_increase_plus1: [0; 7],
        });
        dec_pic_buf_mgr.max_dec_pic_buffering_minus1[0] = max_references as u8;
        dec_pic_buf_mgr.max_latency_increase_plus1[0] = 1;
        dec_pic_buf_mgr.max_num_reorder_pics[0] = 0;

        let dec_pic_buf_mgr = NonNull::from(Box::leak(dec_pic_buf_mgr));

        Self {
            profile_tier_level: Some(profile_tier_level),
            dec_pic_buf_mgr: Some(dec_pic_buf_mgr),
            vps: vk::native::StdVideoH265VideoParameterSet {
                reserved1: 0,
                flags: vk::native::StdVideoH265VpsFlags {
                    _bitfield_align_1: [],
                    _bitfield_1: vk::native::StdVideoH265VpsFlags::new_bitfield_1(1, 1, 0, 0),
                    __bindgen_padding_0: [0; 3],
                },
                vps_video_parameter_set_id: 0,
                vps_max_sub_layers_minus1: 0,
                reserved2: 0,
                vps_num_units_in_tick: 0,
                vps_time_scale: 0,
                vps_num_ticks_poc_diff_one_minus1: 0,
                reserved3: 0,
                pHrdParameters: std::ptr::null(),
                pDecPicBufMgr: dec_pic_buf_mgr.as_ptr(),
                pProfileTierLevel: profile_tier_level.as_ptr() as *const _,
            },
        }
    }
}

impl Drop for VkH265VideoParameterSet {
    fn drop(&mut self) {
        unsafe {
            if let Some(profile_tier_level) = self.profile_tier_level {
                drop(Box::from_raw(profile_tier_level.as_ptr()));
            }

            if let Some(dec_pic_buf_mgr) = self.profile_tier_level {
                drop(Box::from_raw(dec_pic_buf_mgr.as_ptr()));
            }
        }
    }
}

pub(crate) struct VkH265SequenceParameterSet {
    pub(crate) sps: vk::native::StdVideoH265SequenceParameterSet,
}

impl VkH265SequenceParameterSet {
    pub(crate) fn new_encode() -> Self {
        // TODO: VUI
        Self {
            sps: vk::native::StdVideoH265SequenceParameterSet {
                flags: vk::native::StdVideoH265SpsFlags {
                    _bitfield_align_1: [],
                    _bitfield_1: vk::native::StdVideoH265SpsFlags::new_bitfield_1(1, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 1, 1, 0)
                },

            }
        }
    }
}

pub(crate) struct VkH265PictureParameterSet {
    pub(crate) pps: vk::native::StdVideoH265PictureParameterSet,
}

pub(crate) fn vk_to_h265_level_idc(
    level_idc: vk::native::StdVideoH265LevelIdc,
) -> Result<u8, VulkanDecoderError> {
    match level_idc {
        vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_1_0 => Ok(30),
        vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_2_0 => Ok(60),
        vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_2_1 => Ok(63),
        vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_3_0 => Ok(90),
        vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_3_1 => Ok(93),
        vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_4_0 => Ok(120),
        vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_4_1 => Ok(123),
        vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_5_0 => Ok(150),
        vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_5_1 => Ok(153),
        vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_5_2 => Ok(156),
        vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_0 => Ok(180),
        vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_1 => Ok(183),
        vk::native::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2 => Ok(186),
        _ => Err(VulkanDecoderError::InvalidInputData(format!(
            "unknown StdVideoH265LevelIdc: {level_idc}"
        ))),
    }
}
