// Re-exports FFmpeg constants that bindgen cannot evaluate directly (their
// definitions use casts/macro arithmetic). The C compiler evaluates them here
// so bindgen only sees plain static constants.
#include <libavformat/avformat.h>

static const int64_t NIAN_AV_NOPTS_VALUE = AV_NOPTS_VALUE;
static const int NIAN_AVERROR_EOF = AVERROR_EOF;
