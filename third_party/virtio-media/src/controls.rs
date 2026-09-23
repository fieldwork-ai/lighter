//! A fixed set of integer-valued controls, answered as the V4L2 core's control
//! framework answers them: devices here have no kernel control handler behind
//! them, so these rules (class controls, read- and write-only errors, which
//! entry `error_idx` names, initial control events) are theirs to keep, and
//! v4l2-compliance holds them to it. Added for lighter; see
//! third_party/README.md.

use v4l2r::bindings;
use v4l2r::ioctl::CtrlId;
use v4l2r::ioctl::CtrlWhich;
use v4l2r::ioctl::QueryCtrlFlags;
use v4l2r::ioctl::SubscribeEventFlags;

use crate::ioctl::IoctlResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Integer,
    Boolean,
    /// Empty names are entries the device does not offer.
    Menu(&'static [&'static str]),
    Button,
    /// The control that heads a class, as `V4L2_CID_USER_CLASS` does.
    Class,
}

#[derive(Debug)]
pub struct Control {
    pub id: u32,
    pub name: &'static str,
    pub kind: Kind,
    pub min: i64,
    pub max: i64,
    pub default: i64,
    /// Read-only controls are volatile too: the device reports them.
    pub read_only: bool,
}

impl Control {
    pub const fn class(id: u32, name: &'static str) -> Control {
        Control {
            id,
            name,
            kind: Kind::Class,
            min: 0,
            max: 0,
            default: 0,
            read_only: true,
        }
    }

    fn v4l2_type(&self) -> u32 {
        match self.kind {
            Kind::Integer => bindings::v4l2_ctrl_type_V4L2_CTRL_TYPE_INTEGER,
            Kind::Boolean => bindings::v4l2_ctrl_type_V4L2_CTRL_TYPE_BOOLEAN,
            Kind::Menu(_) => bindings::v4l2_ctrl_type_V4L2_CTRL_TYPE_MENU,
            Kind::Button => bindings::v4l2_ctrl_type_V4L2_CTRL_TYPE_BUTTON,
            Kind::Class => bindings::v4l2_ctrl_type_V4L2_CTRL_TYPE_CTRL_CLASS,
        }
    }

    fn flags(&self) -> u32 {
        match self.kind {
            Kind::Class => bindings::V4L2_CTRL_FLAG_READ_ONLY | bindings::V4L2_CTRL_FLAG_WRITE_ONLY,
            Kind::Button => {
                bindings::V4L2_CTRL_FLAG_WRITE_ONLY | bindings::V4L2_CTRL_FLAG_EXECUTE_ON_WRITE
            }
            _ if self.read_only => {
                bindings::V4L2_CTRL_FLAG_READ_ONLY | bindings::V4L2_CTRL_FLAG_VOLATILE
            }
            _ => 0,
        }
    }

    fn readable(&self) -> bool {
        !matches!(self.kind, Kind::Button | Kind::Class)
    }

    fn writable(&self) -> bool {
        !self.read_only
    }

    /// The value as it would be stored. Integers are clamped, as the V4L2
    /// core does; a menu entry out of range is `ERANGE` and one the device
    /// does not offer `EINVAL`.
    pub fn validate(&self, value: i64) -> IoctlResult<i64> {
        match self.kind {
            Kind::Menu(names) => {
                if value < self.min || value > self.max {
                    Err(libc::ERANGE)
                } else if names.get(value as usize).is_some_and(|n| !n.is_empty()) {
                    Ok(value)
                } else {
                    Err(libc::EINVAL)
                }
            }
            Kind::Boolean => Ok((value != 0) as i64),
            Kind::Button | Kind::Class => Ok(0),
            Kind::Integer => Ok(value.clamp(self.min, self.max)),
        }
    }

    fn class_id(&self) -> u32 {
        self.id & 0x0fff_0000
    }
}

/// Every control a device has, sorted by id, which is the order `NEXT` walks
/// them in; a class's control (`0x…0001`) therefore comes before its members.
pub struct Controls(pub &'static [Control]);

/// Which V4L2 events a session holds on its controls.
#[derive(Debug, Default)]
pub struct ControlSubscriptions(Vec<(u32, SubscribeEventFlags)>);

impl Controls {
    pub fn get(&self, id: u32) -> Option<&'static Control> {
        self.0.iter().find(|c| c.id == id)
    }

    pub fn query_ext(
        &self,
        id: CtrlId,
        flags: QueryCtrlFlags,
    ) -> IoctlResult<bindings::v4l2_query_ext_ctrl> {
        let wanted = id.id();
        let c = if flags.contains(QueryCtrlFlags::NEXT) {
            self.0.iter().find(|c| c.id > wanted)
        } else {
            self.get(wanted)
        }
        .ok_or(libc::EINVAL)?;
        let mut name = [0; 32];
        for (d, s) in name.iter_mut().zip(c.name.bytes()) {
            *d = s as _;
        }
        let ranged = !matches!(c.kind, Kind::Button | Kind::Class);
        Ok(bindings::v4l2_query_ext_ctrl {
            id: c.id,
            type_: c.v4l2_type(),
            name,
            minimum: c.min,
            maximum: c.max,
            step: ranged as u64,
            default_value: c.default,
            flags: c.flags(),
            elem_size: 4,
            elems: 1,
            ..Default::default()
        })
    }

    pub fn query(
        &self,
        id: CtrlId,
        flags: QueryCtrlFlags,
    ) -> IoctlResult<bindings::v4l2_queryctrl> {
        let q = self.query_ext(id, flags)?;
        Ok(bindings::v4l2_queryctrl {
            id: q.id,
            type_: q.type_,
            name: q.name.map(|c| c as u8),
            minimum: q.minimum as i32,
            maximum: q.maximum as i32,
            step: q.step as i32,
            default_value: q.default_value as i32,
            flags: q.flags,
            ..Default::default()
        })
    }

    pub fn query_menu(&self, id: u32, index: u32) -> IoctlResult<bindings::v4l2_querymenu> {
        let Some(Control {
            kind: Kind::Menu(names),
            min,
            max,
            ..
        }) = self.get(id)
        else {
            return Err(libc::EINVAL);
        };
        let label = names
            .get(index as usize)
            .filter(|n| !n.is_empty() && (*min..=*max).contains(&i64::from(index)))
            .ok_or(libc::EINVAL)?;
        let mut name = [0u8; 32];
        for (d, s) in name.iter_mut().zip(label.bytes()) {
            *d = s;
        }
        Ok(bindings::v4l2_querymenu {
            id,
            index,
            __bindgen_anon_1: bindings::v4l2_querymenu__bindgen_ty_1 { name },
            reserved: 0,
        })
    }

    /// Resolves every entry, as the core does before touching any value: an
    /// unknown id, or one outside the class `which` names, is `EINVAL` at
    /// its index.
    fn resolve(
        &self,
        which: &CtrlWhich,
        ctrls: &mut bindings::v4l2_ext_controls,
        ctrl_array: &[bindings::v4l2_ext_control],
    ) -> IoctlResult<Vec<&'static Control>> {
        if let CtrlWhich::Class(class) = which {
            if self.get(class | 1).is_none() {
                ctrls.error_idx = ctrls.count;
                return Err(libc::EINVAL);
            }
        }
        if matches!(which, CtrlWhich::Request(_)) {
            ctrls.error_idx = ctrls.count;
            return Err(libc::EINVAL);
        }
        ctrl_array
            .iter()
            .enumerate()
            .map(|(i, ctrl)| {
                self.get(ctrl.id)
                    .filter(|c| !matches!(which, CtrlWhich::Class(class) if c.class_id() != *class))
                    .ok_or_else(|| {
                        ctrls.error_idx = i as u32;
                        libc::EINVAL
                    })
            })
            .collect()
    }

    /// `VIDIOC_G_EXT_CTRLS`, reading current values through `value`.
    pub fn get_values(
        &self,
        which: CtrlWhich,
        ctrls: &mut bindings::v4l2_ext_controls,
        ctrl_array: &mut [bindings::v4l2_ext_control],
        value: impl Fn(&Control) -> i64,
    ) -> IoctlResult<()> {
        let resolved = self.resolve(&which, ctrls, ctrl_array).inspect_err(|_| {
            ctrls.error_idx = ctrls.count;
        })?;
        ctrls.error_idx = ctrls.count;
        if resolved.iter().any(|c| !c.readable()) {
            return Err(libc::EACCES);
        }
        for (ctrl, c) in ctrl_array.iter_mut().zip(resolved) {
            ctrl.__bindgen_anon_1.value = match which {
                CtrlWhich::Default => c.default,
                _ => value(c),
            } as i32;
        }
        Ok(())
    }

    /// `VIDIOC_TRY_EXT_CTRLS`: checks every entry and writes back the values
    /// as they would be stored. A failure names its entry in `error_idx`.
    pub fn try_values(
        &self,
        which: CtrlWhich,
        ctrls: &mut bindings::v4l2_ext_controls,
        ctrl_array: &mut [bindings::v4l2_ext_control],
    ) -> IoctlResult<()> {
        if matches!(which, CtrlWhich::Default) {
            ctrls.error_idx = ctrls.count;
            return Err(libc::EINVAL);
        }
        let resolved = self.resolve(&which, ctrls, ctrl_array)?;
        ctrls.error_idx = ctrls.count;
        for (i, (ctrl, c)) in ctrl_array.iter_mut().zip(resolved).enumerate() {
            ctrls.error_idx = i as u32;
            if !c.writable() {
                return Err(libc::EACCES);
            }
            // SAFETY: every control here is a 32-bit value.
            let v = c.validate(i64::from(unsafe { ctrl.__bindgen_anon_1.value }))?;
            ctrl.__bindgen_anon_1.value = v as i32;
        }
        ctrls.error_idx = ctrls.count;
        Ok(())
    }

    /// `VIDIOC_S_EXT_CTRLS`, all or nothing: on success the entries hold the
    /// values to store. A failed set names no entry (`error_idx` is `count`),
    /// since nothing was applied.
    pub fn set_values(
        &self,
        which: CtrlWhich,
        ctrls: &mut bindings::v4l2_ext_controls,
        ctrl_array: &mut [bindings::v4l2_ext_control],
    ) -> IoctlResult<()> {
        self.try_values(which, ctrls, ctrl_array).inspect_err(|_| {
            ctrls.error_idx = ctrls.count;
        })
    }

    /// `VIDIOC_SUBSCRIBE_EVENT` for `V4L2_EVENT_CTRL`, returning the initial
    /// event, carrying `value`, if one was asked for. A class control is
    /// subscribable but never signals, as in the core.
    pub fn subscribe(
        &self,
        subscriptions: &mut ControlSubscriptions,
        id: u32,
        flags: SubscribeEventFlags,
        value: i64,
    ) -> IoctlResult<Option<bindings::v4l2_event>> {
        let c = self.get(id).ok_or(libc::EINVAL)?;
        subscriptions.0.retain(|(s, _)| *s != id);
        subscriptions.0.push((id, flags));
        if c.kind == Kind::Class || !flags.contains(SubscribeEventFlags::SEND_INITIAL) {
            return Ok(None);
        }
        let mut changes =
            bindings::V4L2_EVENT_CTRL_CH_FLAGS | bindings::V4L2_EVENT_CTRL_CH_DIMENSIONS;
        if c.readable() {
            changes |= bindings::V4L2_EVENT_CTRL_CH_VALUE;
        }
        Ok(Some(self.event(
            c,
            changes,
            if c.readable() { value } else { 0 },
        )))
    }

    /// `id` 0 is every control, as `V4L2_EVENT_ALL` unsubscribes.
    pub fn unsubscribe(subscriptions: &mut ControlSubscriptions, id: u32) {
        subscriptions.0.retain(|(s, _)| id != 0 && *s != id);
    }

    /// Events for controls a session set itself, which only subscriptions that
    /// asked for feedback receive.
    pub fn feedback(
        &self,
        subscriptions: &ControlSubscriptions,
        set: &[bindings::v4l2_ext_control],
    ) -> Vec<bindings::v4l2_event> {
        set.iter()
            .filter(|ctrl| {
                subscriptions.0.iter().any(|(id, flags)| {
                    *id == ctrl.id && flags.contains(SubscribeEventFlags::ALLOW_FEEDBACK)
                })
            })
            .filter_map(|ctrl| {
                let c = self.get(ctrl.id).filter(|c| c.readable())?;
                // SAFETY: as in `try_values`.
                let value = i64::from(unsafe { ctrl.__bindgen_anon_1.value });
                Some(self.event(c, bindings::V4L2_EVENT_CTRL_CH_VALUE, value))
            })
            .collect()
    }

    fn event(&self, c: &Control, changes: u32, value: i64) -> bindings::v4l2_event {
        let ranged = !matches!(c.kind, Kind::Button | Kind::Class);
        bindings::v4l2_event {
            type_: bindings::V4L2_EVENT_CTRL,
            id: c.id,
            u: bindings::v4l2_event__bindgen_ty_1 {
                ctrl: bindings::v4l2_event_ctrl {
                    changes,
                    type_: c.v4l2_type(),
                    __bindgen_anon_1: bindings::v4l2_event_ctrl__bindgen_ty_1 {
                        value: value as i32,
                    },
                    flags: c.flags(),
                    minimum: c.min as i32,
                    maximum: c.max as i32,
                    step: ranged as i32,
                    default_value: c.default as i32,
                },
            },
            // SAFETY: plain data, for which all zeroes is valid.
            ..unsafe { std::mem::zeroed() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MENU: &[&str] = &["Zero", "", "Two"];
    static TABLE: Controls = Controls(&[
        Control::class(bindings::V4L2_CID_USER_CLASS, "User Controls"),
        Control {
            id: bindings::V4L2_CID_MIN_BUFFERS_FOR_CAPTURE,
            name: "Min Number of Capture Buffers",
            kind: Kind::Integer,
            min: 1,
            max: 32,
            default: 1,
            read_only: true,
        },
        Control::class(bindings::V4L2_CID_CODEC_CLASS, "Codec Controls"),
        Control {
            id: bindings::V4L2_CID_MPEG_VIDEO_GOP_SIZE,
            name: "GOP Size",
            kind: Kind::Integer,
            min: 0,
            max: 100,
            default: 30,
            read_only: false,
        },
        Control {
            id: bindings::V4L2_CID_MPEG_VIDEO_BITRATE_MODE,
            name: "Bitrate Mode",
            kind: Kind::Menu(MENU),
            min: 0,
            max: 2,
            default: 0,
            read_only: false,
        },
        Control {
            id: bindings::V4L2_CID_MPEG_VIDEO_FORCE_KEY_FRAME,
            name: "Force Key Frame",
            kind: Kind::Button,
            min: 0,
            max: 0,
            default: 0,
            read_only: false,
        },
    ]);

    fn ext(
        which: u32,
        ids: &[(u32, i32)],
    ) -> (bindings::v4l2_ext_controls, Vec<bindings::v4l2_ext_control>) {
        let mut ctrls: bindings::v4l2_ext_controls = Default::default();
        ctrls.__bindgen_anon_1.which = which;
        ctrls.count = ids.len() as u32;
        let array = ids
            .iter()
            .map(|&(id, value)| bindings::v4l2_ext_control {
                id,
                __bindgen_anon_1: bindings::v4l2_ext_control__bindgen_ty_1 { value },
                ..Default::default()
            })
            .collect();
        (ctrls, array)
    }

    fn which(ctrls: &bindings::v4l2_ext_controls) -> CtrlWhich {
        CtrlWhich::try_from(ctrls).unwrap()
    }

    #[test]
    fn the_table_is_sorted_with_each_class_first() {
        for pair in TABLE.0.windows(2) {
            assert!(pair[0].id < pair[1].id);
        }
        let mut walked = vec![];
        let mut id = 0;
        while let Ok(q) = TABLE.query_ext(CtrlId::new(id).unwrap(), QueryCtrlFlags::NEXT) {
            walked.push(q.id);
            id = q.id;
        }
        assert_eq!(walked, TABLE.0.iter().map(|c| c.id).collect::<Vec<_>>());
    }

    #[test]
    fn class_and_button_controls_report_no_range() {
        for id in [
            bindings::V4L2_CID_USER_CLASS,
            bindings::V4L2_CID_MPEG_VIDEO_FORCE_KEY_FRAME,
        ] {
            let q = TABLE
                .query_ext(CtrlId::new(id).unwrap(), QueryCtrlFlags::empty())
                .unwrap();
            assert_eq!(
                (q.minimum, q.maximum, q.step, q.default_value),
                (0, 0, 0, 0)
            );
        }
        let class = TABLE
            .query_ext(
                CtrlId::new(bindings::V4L2_CID_USER_CLASS).unwrap(),
                QueryCtrlFlags::empty(),
            )
            .unwrap();
        assert_eq!(
            class.flags,
            bindings::V4L2_CTRL_FLAG_READ_ONLY | bindings::V4L2_CTRL_FLAG_WRITE_ONLY
        );
    }

    #[test]
    fn read_only_controls_refuse_try_at_their_index_and_set_at_count() {
        let id = bindings::V4L2_CID_MIN_BUFFERS_FOR_CAPTURE;
        let (mut ctrls, mut array) = ext(0, &[(id, 4)]);
        assert_eq!(
            TABLE.try_values(which(&ctrls), &mut ctrls, &mut array),
            Err(libc::EACCES)
        );
        assert_eq!(ctrls.error_idx, 0);
        assert_eq!(
            TABLE.set_values(which(&ctrls), &mut ctrls, &mut array),
            Err(libc::EACCES)
        );
        assert_eq!(ctrls.error_idx, 1);
    }

    #[test]
    fn write_only_controls_refuse_get_at_count() {
        let (mut ctrls, mut array) = ext(0, &[(bindings::V4L2_CID_MPEG_VIDEO_FORCE_KEY_FRAME, 0)]);
        assert_eq!(
            TABLE.get_values(which(&ctrls), &mut ctrls, &mut array, |_| 0),
            Err(libc::EACCES)
        );
        assert_eq!(ctrls.error_idx, 1);
    }

    #[test]
    fn no_controls_is_a_valid_request() {
        let (mut ctrls, mut array) = ext(0, &[]);
        assert_eq!(
            TABLE.try_values(which(&ctrls), &mut ctrls, &mut array),
            Ok(())
        );
        assert_eq!(
            TABLE.set_values(which(&ctrls), &mut ctrls, &mut array),
            Ok(())
        );
        assert_eq!(
            TABLE.get_values(which(&ctrls), &mut ctrls, &mut array, |_| 0),
            Ok(())
        );
    }

    #[test]
    fn a_class_request_takes_only_that_class() {
        let gop = bindings::V4L2_CID_MPEG_VIDEO_GOP_SIZE;
        let (mut ctrls, mut array) = ext(bindings::V4L2_CTRL_CLASS_CODEC, &[(gop, 500)]);
        assert_eq!(
            TABLE.try_values(which(&ctrls), &mut ctrls, &mut array),
            Ok(())
        );
        assert_eq!(unsafe { array[0].__bindgen_anon_1.value }, 100);

        let (mut ctrls, mut array) = ext(bindings::V4L2_CTRL_CLASS_USER, &[(gop, 5)]);
        assert_eq!(
            TABLE.try_values(which(&ctrls), &mut ctrls, &mut array),
            Err(libc::EINVAL)
        );
        assert_eq!(ctrls.error_idx, 0);

        let (mut ctrls, mut array) = ext(bindings::V4L2_CTRL_CLASS_CAMERA, &[]);
        assert_eq!(
            TABLE.get_values(which(&ctrls), &mut ctrls, &mut array, |_| 0),
            Err(libc::EINVAL)
        );
    }

    #[test]
    fn defaults_can_be_read_but_not_written() {
        let gop = bindings::V4L2_CID_MPEG_VIDEO_GOP_SIZE;
        let (mut ctrls, mut array) = ext(bindings::V4L2_CTRL_WHICH_DEF_VAL, &[(gop, 1)]);
        assert_eq!(
            TABLE.get_values(which(&ctrls), &mut ctrls, &mut array, |_| 7),
            Ok(())
        );
        assert_eq!(unsafe { array[0].__bindgen_anon_1.value }, 30);
        assert_eq!(
            TABLE.set_values(which(&ctrls), &mut ctrls, &mut array),
            Err(libc::EINVAL)
        );
    }

    #[test]
    fn menus_refuse_entries_they_do_not_offer() {
        let mode = bindings::V4L2_CID_MPEG_VIDEO_BITRATE_MODE;
        for (value, expected) in [(2, Ok(())), (1, Err(libc::EINVAL)), (3, Err(libc::ERANGE))] {
            let (mut ctrls, mut array) = ext(0, &[(mode, value)]);
            assert_eq!(
                TABLE.try_values(which(&ctrls), &mut ctrls, &mut array),
                expected
            );
        }
        assert!(TABLE.query_menu(mode, 1).is_err());
        assert!(TABLE.query_menu(mode, 2).is_ok());
        assert!(TABLE
            .query_menu(bindings::V4L2_CID_MPEG_VIDEO_GOP_SIZE, 0)
            .is_err());
    }

    #[test]
    fn subscriptions_send_initial_values_but_never_for_a_class() {
        let mut subs = ControlSubscriptions::default();
        let gop = bindings::V4L2_CID_MPEG_VIDEO_GOP_SIZE;
        let ev = TABLE
            .subscribe(&mut subs, gop, SubscribeEventFlags::SEND_INITIAL, 12)
            .unwrap()
            .unwrap();
        assert_eq!((ev.type_, ev.id), (bindings::V4L2_EVENT_CTRL, gop));
        assert_eq!(unsafe { ev.u.ctrl.__bindgen_anon_1.value }, 12);
        assert!(matches!(
            TABLE.subscribe(
                &mut subs,
                bindings::V4L2_CID_USER_CLASS,
                SubscribeEventFlags::SEND_INITIAL,
                0
            ),
            Ok(None)
        ));
        assert_eq!(
            TABLE
                .subscribe(
                    &mut subs,
                    0x00980000 | 0x999,
                    SubscribeEventFlags::empty(),
                    0
                )
                .err(),
            Some(libc::EINVAL)
        );
    }

    #[test]
    fn feedback_goes_only_to_subscriptions_that_asked() {
        let mut subs = ControlSubscriptions::default();
        let gop = bindings::V4L2_CID_MPEG_VIDEO_GOP_SIZE;
        TABLE
            .subscribe(&mut subs, gop, SubscribeEventFlags::empty(), 0)
            .unwrap();
        let (_, set) = ext(0, &[(gop, 9)]);
        assert!(TABLE.feedback(&subs, &set).is_empty());
        TABLE
            .subscribe(&mut subs, gop, SubscribeEventFlags::ALLOW_FEEDBACK, 0)
            .unwrap();
        assert_eq!(TABLE.feedback(&subs, &set).len(), 1);
        Controls::unsubscribe(&mut subs, 0);
        assert!(TABLE.feedback(&subs, &set).is_empty());
    }
}
