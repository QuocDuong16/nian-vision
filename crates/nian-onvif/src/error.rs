use thiserror::Error;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum OnvifError {
    #[error("ONVIF operation already in progress")]
    Busy,
    #[error("ONVIF operation timed out")]
    Timeout,
    #[error("ONVIF device is unreachable")]
    DeviceUnreachable,
    #[error("ONVIF authentication failed")]
    AuthFailed,
    #[error("malformed or unsupported ONVIF response")]
    Protocol,
    #[error("ONVIF response exceeded the configured limit")]
    ResponseTooLarge,
    #[error("ONVIF capability is unsupported")]
    Unsupported,
    #[error("ONVIF Events service is unavailable")]
    EventServiceUnsupported,
    #[error("ONVIF Events service does not advertise a compatible motion topic")]
    MotionEventUnsupported,
    #[error("no compatible H.264 media profile is available")]
    NoCompatibleProfile,
    #[error("camera returned an invalid RTSP stream URI")]
    InvalidStreamUri,
    #[error("camera returned an unsafe or unrelated network authority")]
    AuthorityRejected,
    #[error("ONVIF operation was cancelled")]
    Cancelled,
    #[error("ONVIF internal operation failed")]
    Internal,
}
