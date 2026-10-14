#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublicV1ControllerRegistration {
    Reconciler,
    #[allow(dead_code)]
    Alternate,
}

impl PublicV1ControllerRegistration {
    #[allow(dead_code)]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Reconciler => "reconciler",
            Self::Alternate => "alternate_controller_owner",
        }
    }
}

pub(crate) const PUBLIC_V1_CONTROLLER_REGISTRATIONS: &[PublicV1ControllerRegistration] =
    &[PublicV1ControllerRegistration::Reconciler];

pub(crate) fn validate_public_v1_controller_registrations(
    registrations: &[PublicV1ControllerRegistration],
) -> Result<(), &'static str> {
    if registrations != [PublicV1ControllerRegistration::Reconciler] {
        return Err("Public V1 controller registrations changed");
    }
    Ok(())
}
