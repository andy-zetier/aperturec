use crate::{common, control, event};

use aperturec_graphics::display::*;
use aperturec_graphics::prelude::*;

use std::num::TryFromIntError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("failed parsing SemVer")]
    ParseSemver(#[from] semver::Error),
    #[error("missing field")]
    MissingField(&'static str),
    #[error("integer conversion")]
    IntegerConversion(#[from] TryFromIntError),
    #[error("decoder assigned to disabled display")]
    DecoderAssignedToDisabledDisplay,
}

pub type Result<T> = std::result::Result<T, Error>;

impl TryFrom<&str> for common::SemVer {
    type Error = Error;

    fn try_from(s: &str) -> Result<Self> {
        let sv = semver::Version::parse(s)?;
        Ok(sv.into())
    }
}

impl common::SemVer {
    pub fn from_cargo() -> Result<Self> {
        env!("CARGO_PKG_VERSION").try_into()
    }
}

impl From<semver::Version> for common::SemVer {
    fn from(sv: semver::Version) -> Self {
        Self {
            major: sv.major,
            minor: sv.minor,
            patch: sv.patch,
            pre_release: sv.pre.to_string(),
            build_metadata: sv.build.to_string(),
        }
    }
}

impl TryFrom<common::SemVer> for semver::Version {
    type Error = Error;

    fn try_from(sv: common::SemVer) -> Result<Self> {
        let mut v = semver::Version::new(sv.major, sv.minor, sv.patch);
        v.pre = sv.pre_release.parse().unwrap_or_default();
        v.build = sv.build_metadata.parse().unwrap_or_default();
        Ok(v)
    }
}

impl TryFrom<common::Rectangle> for Rect {
    type Error = Error;

    fn try_from(proto_rect: common::Rectangle) -> Result<Self> {
        let location = proto_rect.location.ok_or(Error::MissingField("location"))?;
        let dimension = proto_rect
            .dimension
            .ok_or(Error::MissingField("dimension"))?;
        Ok(Rect::new(
            Point::new(
                location.x_position.try_into()?,
                location.y_position.try_into()?,
            ),
            Size::new(dimension.width.try_into()?, dimension.height.try_into()?),
        ))
    }
}

impl TryFrom<common::Rectangle> for Box2D {
    type Error = Error;

    fn try_from(proto_rect: common::Rectangle) -> Result<Self> {
        Ok(<common::Rectangle as TryInto<Rect>>::try_into(proto_rect)?.to_box2d())
    }
}

impl TryFrom<Rect> for common::Rectangle {
    type Error = Error;

    fn try_from(rect: Rect) -> Result<Self> {
        Ok(common::Rectangle {
            location: Some(common::Location {
                x_position: rect.origin.x.try_into()?,
                y_position: rect.origin.y.try_into()?,
            }),
            dimension: Some(common::Dimension {
                width: rect.size.width.try_into()?,
                height: rect.size.height.try_into()?,
            }),
        })
    }
}

impl TryFrom<Box2D> for common::Rectangle {
    type Error = Error;

    fn try_from(box2d: Box2D) -> Result<Self> {
        <Rect as TryInto<common::Rectangle>>::try_into(box2d.to_rect())
    }
}

impl common::Rectangle {
    pub fn try_from_size_at_origin(size: Size) -> Result<Self> {
        Self::try_from(Rect::from_size(size))
    }
}

impl TryFrom<Size> for common::Dimension {
    type Error = Error;

    fn try_from(size: Size) -> Result<Self> {
        Ok(common::Dimension {
            width: size.width.try_into()?,
            height: size.height.try_into()?,
        })
    }
}

impl TryFrom<common::Dimension> for Size {
    type Error = Error;

    fn try_from(dim: common::Dimension) -> Result<Self> {
        Ok(Size::new(dim.width.try_into()?, dim.height.try_into()?))
    }
}

impl TryFrom<Point> for common::Location {
    type Error = Error;

    fn try_from(point: Point) -> Result<Self> {
        Ok(common::Location {
            x_position: point.x.try_into()?,
            y_position: point.y.try_into()?,
        })
    }
}

impl TryFrom<common::Location> for Point {
    type Error = Error;

    fn try_from(loc: common::Location) -> Result<Self> {
        Ok(Point::new(
            loc.x_position.try_into()?,
            loc.y_position.try_into()?,
        ))
    }
}

impl TryFrom<Display> for common::DisplayInfo {
    type Error = Error;

    fn try_from(display: Display) -> Result<Self> {
        Ok(common::DisplayInfo {
            area: Some(display.area.try_into()?),
            is_enabled: display.is_enabled,
        })
    }
}

impl TryFrom<common::DisplayConfiguration> for DisplayConfiguration {
    type Error = Error;
    fn try_from(display_config: common::DisplayConfiguration) -> Result<DisplayConfiguration> {
        Ok(DisplayConfiguration {
            id: display_config.id.try_into()?,
            display_decoder_infos: display_config
                .display_decoder_infos
                .into_iter()
                .map(DisplayDecoderInfo::try_from)
                .collect::<Result<_>>()?,
        })
    }
}

impl TryFrom<common::DisplayDecoderInfo> for DisplayDecoderInfo {
    type Error = Error;
    fn try_from(ddi: common::DisplayDecoderInfo) -> Result<DisplayDecoderInfo> {
        let display: Display = ddi
            .display
            .ok_or(Error::MissingField("display"))?
            .try_into()?;
        if !display.is_enabled {
            if !ddi.decoder_areas.is_empty() {
                Err(Error::DecoderAssignedToDisabledDisplay)
            } else {
                Ok(DisplayDecoderInfo {
                    display,
                    decoder_areas: Box::new([]),
                })
            }
        } else {
            Ok(DisplayDecoderInfo {
                display,
                decoder_areas: ddi
                    .decoder_areas
                    .into_iter()
                    .map(Rect::try_from)
                    .collect::<Result<_>>()?,
            })
        }
    }
}

impl TryFrom<common::DisplayInfo> for Display {
    type Error = Error;

    fn try_from(disp: common::DisplayInfo) -> Result<Self> {
        let area = disp.area.ok_or(Error::MissingField("area"))?;
        Ok(Display {
            area: area.try_into()?,
            is_enabled: disp.is_enabled,
        })
    }
}

impl AsRef<[common::DisplayInfo]> for control::ClientInfo {
    fn as_ref(&self) -> &[common::DisplayInfo] {
        &self.displays
    }
}

impl AsRef<[common::DisplayInfo]> for event::DisplayEvent {
    fn as_ref(&self) -> &[common::DisplayInfo] {
        &self.displays
    }
}
