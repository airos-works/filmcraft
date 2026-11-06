/*
 * Movies and sounds FilmCraft's own decoders don't read, through the Media
 * Kit's readers and decoders (bound in src/media_kit.rs): Windows Media, Flash
 * Video, DV, RealMedia, MPEG-4 Part 2 (DivX, Xvid), Theora and whatever else the
 * system's codecs (the ffmpeg add-on) read. The file comes through FilmCraft's
 * own reader (a callback), as a BPositionIO; video frames are decoded to
 * B_RGB32 and audio to interleaved 32-bit floats.
 */

#include <DataIO.h>
#include <MediaDefs.h>
#include <MediaFile.h>
#include <MediaTrack.h>

#include <new>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vector>


extern "C" {

// Reads `length` bytes at `offset`; how many it read, or a negative error.
typedef int64_t (*FcRead)(void* cookie, int64_t offset, void* buffer,
	size_t length);

struct FcMediaInfo {
	int32_t	has_video;
	int32_t	has_audio;
	int32_t	video_decodable;
	int32_t	audio_decodable;
	int32_t	width;
	int32_t	height;
	float	fps;
	int32_t	channels;
	float	rate;
	int32_t	par_x;
	int32_t	par_y;
	int64_t	duration_us;
	int64_t	video_frames;
	int64_t	audio_frames;
	char	container[64];
	char	video_codec[96];
	char	audio_codec[96];
};

}


class CallbackIO : public BPositionIO {
public:
	CallbackIO(FcRead read, void* cookie, off_t size)
		:
		fRead(read),
		fCookie(cookie),
		fSize(size),
		fPosition(0)
	{
	}

	virtual ssize_t ReadAt(off_t position, void* buffer, size_t size)
	{
		if (position < 0)
			return B_BAD_VALUE;
		if (position >= fSize)
			return 0;
		if ((off_t)size > fSize - position)
			size = fSize - position;
		int64_t read = fRead(fCookie, position, buffer, size);
		return read < 0 ? B_IO_ERROR : (ssize_t)read;
	}

	virtual ssize_t WriteAt(off_t, const void*, size_t)
	{
		return B_NOT_ALLOWED;
	}

	virtual off_t Seek(off_t position, uint32 mode)
	{
		switch (mode) {
			case SEEK_SET: fPosition = position; break;
			case SEEK_CUR: fPosition += position; break;
			case SEEK_END: fPosition = fSize + position; break;
			default: return B_BAD_VALUE;
		}
		return fPosition;
	}

	virtual off_t Position() const
	{
		return fPosition;
	}

	virtual status_t GetSize(off_t* size) const
	{
		*size = fSize;
		return B_OK;
	}

	virtual status_t SetSize(off_t)
	{
		return B_NOT_ALLOWED;
	}

private:
	FcRead	fRead;
	void*	fCookie;
	off_t	fSize;
	off_t	fPosition;
};


struct FcMedia {
	CallbackIO*		io;
	BMediaFile*		file;
	BMediaTrack*	video;
	BMediaTrack*	audio;
	int32			width;
	int32			height;
	size_t			rowBytes;
	size_t			videoBufferSize;
	// The last picture decoded and its time (-1: none since the last seek).
	std::vector<uint8>	picture;
	bigtime_t		pictureTime;
	// The next picture, decoded but not shown yet (-1: none).
	std::vector<uint8>	pending;
	bigtime_t		pendingTime;
	bigtime_t		frameDuration;
	// Audio: the next frame ReadFrames gives, and what a decode gave
	// beyond what was asked.
	int32			channels;
	float			rate;
	size_t			audioBufferSize;
	int64			audioNext;
	std::vector<float>	audioLeft;
	int64			audioLeftStart;
};


static void
CodecName(BMediaTrack* track, char* out, size_t length)
{
	media_codec_info info;
	if (track->GetCodecInfo(&info) == B_OK)
		snprintf(out, length, "%s", info.pretty_name);
	else
		snprintf(out, length, "unknown");
}


extern "C" void*
fc_media_open(FcRead read, void* cookie, int64_t size, FcMediaInfo* info)
{
	memset(info, 0, sizeof(*info));
	FcMedia* media = new(std::nothrow) FcMedia();
	if (media == NULL)
		return NULL;
	media->io = new(std::nothrow) CallbackIO(read, cookie, size);
	media->file = media->io != NULL
		? new(std::nothrow) BMediaFile(media->io) : NULL;
	media->video = NULL;
	media->audio = NULL;
	media->pictureTime = -1;
	media->pendingTime = -1;
	media->audioNext = -1;
	media->audioLeftStart = 0;
	if (media->file == NULL || media->file->InitCheck() != B_OK) {
		delete media->file;
		delete media->io;
		delete media;
		return NULL;
	}
	media_file_format fileFormat;
	if (media->file->GetFileFormatInfo(&fileFormat) == B_OK)
		snprintf(info->container, sizeof(info->container), "%s",
			fileFormat.short_name);
	for (int32 i = 0; i < media->file->CountTracks(); i++) {
		BMediaTrack* track = media->file->TrackAt(i);
		if (track == NULL)
			continue;
		media_format encoded;
		track->EncodedFormat(&encoded);
		if (encoded.IsVideo() && media->video == NULL) {
			media->video = track;
			info->has_video = 1;
			CodecName(track, info->video_codec, sizeof(info->video_codec));
			media_format format;
			format.type = B_MEDIA_RAW_VIDEO;
			format.u.raw_video = media_raw_video_format::wildcard;
			format.u.raw_video.display.format = B_RGB32;
			if (track->DecodedFormat(&format) == B_OK
				&& format.u.raw_video.display.format == B_RGB32) {
				media_video_display_info& display = format.u.raw_video.display;
				media->width = display.line_width;
				media->height = display.line_count;
				media->rowBytes = display.bytes_per_row > 0
					? display.bytes_per_row : (size_t)media->width * 4;
				media->videoBufferSize = media->rowBytes * media->height;
				info->video_decodable = media->width > 0 && media->height > 0
					&& media->width <= 16384 && media->height <= 16384
					&& media->rowBytes >= (size_t)media->width * 4;
				info->width = media->width;
				info->height = media->height;
				info->fps = format.u.raw_video.field_rate;
				info->par_x = format.u.raw_video.pixel_width_aspect;
				info->par_y = format.u.raw_video.pixel_height_aspect;
			}
			info->video_frames = track->CountFrames();
			bigtime_t duration = track->Duration();
			if (duration > info->duration_us)
				info->duration_us = duration;
			media->frameDuration = info->fps > 0
				? (bigtime_t)(1000000.0 / info->fps) : 40000;
		} else if (encoded.IsAudio() && media->audio == NULL) {
			media->audio = track;
			info->has_audio = 1;
			CodecName(track, info->audio_codec, sizeof(info->audio_codec));
			media_format format;
			format.type = B_MEDIA_RAW_AUDIO;
			format.u.raw_audio = media_raw_audio_format::wildcard;
			format.u.raw_audio.format = media_raw_audio_format::B_AUDIO_FLOAT;
			format.u.raw_audio.byte_order = B_MEDIA_HOST_ENDIAN;
			if (track->DecodedFormat(&format) == B_OK
				&& format.u.raw_audio.format
					== media_raw_audio_format::B_AUDIO_FLOAT
				&& format.u.raw_audio.channel_count > 0
				&& format.u.raw_audio.channel_count <= 32
				&& format.u.raw_audio.frame_rate > 0) {
				media->channels = format.u.raw_audio.channel_count;
				media->rate = format.u.raw_audio.frame_rate;
				media->audioBufferSize = format.u.raw_audio.buffer_size > 0
					? format.u.raw_audio.buffer_size : 65536;
				info->audio_decodable = 1;
				info->channels = media->channels;
				info->rate = media->rate;
			}
			info->audio_frames = track->CountFrames();
			bigtime_t duration = track->Duration();
			if (duration > info->duration_us)
				info->duration_us = duration;
		} else {
			media->file->ReleaseTrack(track);
		}
	}
	if (!info->video_decodable && media->video != NULL) {
		media->file->ReleaseTrack(media->video);
		media->video = NULL;
	}
	if (!info->audio_decodable && media->audio != NULL) {
		media->file->ReleaseTrack(media->audio);
		media->audio = NULL;
	}
	return media;
}


// The picture shown at `time`: the last one starting at or before it (the
// first one, before the first). 0 and the picture in `out` (rowBytes apart,
// B_RGB32), its time in `shown`.
extern "C" int32_t
fc_media_video(void* handle, int64_t time, uint8_t* out, size_t outLength,
	int64_t* shown)
{
	FcMedia* media = static_cast<FcMedia*>(handle);
	if (media->video == NULL || outLength < media->videoBufferSize)
		return B_BAD_VALUE;
	if (time < 0)
		time = 0;
	// Forward from the last picture when the time is soon after it; else
	// from the key frame before it.
	bool forward = media->pictureTime >= 0 && time >= media->pictureTime
		&& time - media->pictureTime < 2000000;
	if (!forward) {
		bigtime_t at = time;
		status_t status = media->video->SeekToTime(&at,
			B_MEDIA_SEEK_CLOSEST_BACKWARD);
		if (status != B_OK)
			return status;
		media->pictureTime = -1;
		media->pendingTime = -1;
	}
	media->picture.resize(media->videoBufferSize);
	media->pending.resize(media->videoBufferSize);
	for (int guard = 0; guard < 100000; guard++) {
		if (media->pictureTime >= 0
			&& time < media->pictureTime + media->frameDuration)
			break;
		if (media->pendingTime < 0) {
			int64 count = 0;
			media_header header;
			status_t status = media->video->ReadFrames(media->pending.data(),
				&count, &header);
			if (status != B_OK || count < 1) {
				// The end: the last picture stays.
				if (media->pictureTime >= 0)
					break;
				return status != B_OK ? status : B_LAST_BUFFER_ERROR;
			}
			media->pendingTime = header.start_time;
		}
		// A picture after the time: the current one is shown, this one
		// waits for a later time.
		if (media->pictureTime >= 0 && media->pendingTime > time)
			break;
		media->picture.swap(media->pending);
		media->pictureTime = media->pendingTime;
		media->pendingTime = -1;
	}
	memcpy(out, media->picture.data(), media->videoBufferSize);
	*shown = media->pictureTime;
	return B_OK;
}


// `count` frames of audio from frame `start` (at the file's rate), as
// interleaved floats; how many it gave (fewer at the end).
extern "C" int64_t
fc_media_audio(void* handle, int64_t start, float* out, int64_t count)
{
	FcMedia* media = static_cast<FcMedia*>(handle);
	if (media->audio == NULL || count <= 0)
		return 0;
	int32 channels = media->channels;
	int64 given = 0;
	// What the last decode gave beyond what was asked.
	int64 leftFrames = (int64)media->audioLeft.size() / channels;
	if (start >= media->audioLeftStart
		&& start < media->audioLeftStart + leftFrames) {
		int64 skip = start - media->audioLeftStart;
		int64 take = leftFrames - skip < count ? leftFrames - skip : count;
		memcpy(out, media->audioLeft.data() + skip * channels,
			take * channels * sizeof(float));
		given = take;
		if (given == count)
			return given;
	}
	int64 want = start + given;
	if (media->audioNext != want) {
		int64 frame = want;
		if (media->audio->SeekToFrame(&frame, B_MEDIA_SEEK_CLOSEST_BACKWARD)
				!= B_OK)
			return given;
		media->audioNext = frame;
	}
	// ReadFrames fills one decoder buffer (the negotiated buffer_size); room
	// for twice the larger of it and 64 KiB.
	size_t bufferBytes = media->audioBufferSize > 65536
		? media->audioBufferSize : 65536;
	std::vector<float> buffer(bufferBytes / sizeof(float) * 2);
	for (int guard = 0; guard < 1000000 && given < count; guard++) {
		int64 frames = 0;
		media_header header;
		if (media->audio->ReadFrames(buffer.data(), &frames, &header) != B_OK
			|| frames <= 0) {
			break;
		}
		if ((size_t)(frames * channels) > buffer.size())
			frames = buffer.size() / channels;
		int64 first = media->audioNext;
		media->audioNext += frames;
		if (first + frames <= want)
			continue;
		int64 skip = want > first ? want - first : 0;
		int64 take = frames - skip;
		if (take > count - given)
			take = count - given;
		memcpy(out + given * channels, buffer.data() + skip * channels,
			take * channels * sizeof(float));
		given += take;
		want += take;
		if (skip + take < frames) {
			media->audioLeft.assign(buffer.data() + (skip + take) * channels,
				buffer.data() + frames * channels);
			media->audioLeftStart = want;
		} else {
			media->audioLeft.clear();
		}
	}
	return given;
}


extern "C" size_t
fc_media_row_bytes(void* handle)
{
	return static_cast<FcMedia*>(handle)->rowBytes;
}


extern "C" void
fc_media_close(void* handle)
{
	FcMedia* media = static_cast<FcMedia*>(handle);
	if (media == NULL)
		return;
	if (media->video != NULL)
		media->file->ReleaseTrack(media->video);
	if (media->audio != NULL)
		media->file->ReleaseTrack(media->audio);
	delete media->file;
	delete media->io;
	delete media;
}
