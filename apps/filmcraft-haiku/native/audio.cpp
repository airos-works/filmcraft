/*
 * FilmCraft's sound through the Media Kit (bound in src/audio.rs and
 * src/audio_in.rs): playback is a BSoundPlayer node feeding the system mixer,
 * whose buffer callback runs FilmCraft's play-ahead mixer, and voice-over
 * recording is a BMediaRecorder connected to the system's audio input (the
 * Media preferences' Audio input), whose buffers arrive as 32-bit floats.
 * Both nodes show in Cortex and the Media preferences while they exist.
 */

#include <MediaDefs.h>
#include <MediaRecorder.h>
#include <SoundPlayer.h>

#include <new>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>


extern "C" {

typedef void (*FcAudioFill)(void* cookie, float* samples, size_t count,
	int32_t channels);
typedef void (*FcRecordSink)(void* cookie, const float* samples, size_t count,
	int32_t channels, float rate);

}


static void
Error(char* error, size_t length, const char* text, status_t status)
{
	if (error != NULL && length > 0)
		snprintf(error, length, "%s: %s", text, strerror(status));
}


// #pragma mark - Playback


struct FcAudio {
	BSoundPlayer*	player;
	FcAudioFill		fill;
	void*			cookie;
};


static void
PlayBuffer(void* cookie, void* buffer, size_t bytes,
	const media_raw_audio_format& format)
{
	FcAudio* audio = static_cast<FcAudio*>(cookie);
	if (format.format != media_raw_audio_format::B_AUDIO_FLOAT
		|| format.channel_count == 0) {
		memset(buffer, 0, bytes);
		return;
	}
	audio->fill(audio->cookie, static_cast<float*>(buffer),
		bytes / sizeof(float), (int32_t)format.channel_count);
}


extern "C" void*
fc_audio_open(float* rate, int32_t* channels, int32_t bufferFrames,
	FcAudioFill fill, void* cookie, char* error, size_t errorLength)
{
	FcAudio* audio = new(std::nothrow) FcAudio;
	if (audio == NULL)
		return NULL;
	audio->fill = fill;
	audio->cookie = cookie;
	media_raw_audio_format format = media_raw_audio_format::wildcard;
	format.format = media_raw_audio_format::B_AUDIO_FLOAT;
	format.channel_count = *channels > 0 ? *channels : 2;
	format.frame_rate = *rate > 0 ? *rate : 48000;
	format.byte_order = B_MEDIA_HOST_ENDIAN;
	if (bufferFrames > 0)
		format.buffer_size = bufferFrames * format.channel_count * sizeof(float);
	audio->player = new(std::nothrow) BSoundPlayer(&format, "FilmCraft",
		PlayBuffer, NULL, audio);
	status_t status = audio->player != NULL ? audio->player->InitCheck()
		: B_NO_MEMORY;
	if (status != B_OK) {
		Error(error, errorLength, "the sound player couldn't start", status);
		delete audio->player;
		delete audio;
		return NULL;
	}
	media_raw_audio_format real = audio->player->Format();
	if (real.format != media_raw_audio_format::B_AUDIO_FLOAT) {
		Error(error, errorLength, "the system mixer refused float samples",
			B_MEDIA_BAD_FORMAT);
		delete audio->player;
		delete audio;
		return NULL;
	}
	*rate = real.frame_rate;
	*channels = (int32_t)real.channel_count;
	return audio;
}


extern "C" int32_t
fc_audio_start(void* handle)
{
	FcAudio* audio = static_cast<FcAudio*>(handle);
	status_t status = audio->player->Start();
	if (status == B_OK)
		audio->player->SetHasData(true);
	return status;
}


// Stops the node, waiting for its buffer callback to return: the cookie may
// be freed afterwards.
extern "C" void
fc_audio_close(void* handle)
{
	FcAudio* audio = static_cast<FcAudio*>(handle);
	if (audio == NULL)
		return;
	audio->player->SetHasData(false);
	audio->player->Stop(true, true);
	delete audio->player;
	delete audio;
}


// How long a buffer takes from the callback to the speakers.
extern "C" int64_t
fc_audio_latency(void* handle)
{
	FcAudio* audio = static_cast<FcAudio*>(handle);
	return audio != NULL ? audio->player->Latency() : 0;
}


// #pragma mark - Recording


struct FcRecord {
	BMediaRecorder*	recorder;
	FcRecordSink	sink;
	void*			cookie;
	float*			scratch;
	size_t			scratchLength;
};


static void
Recorded(void* cookie, bigtime_t /*timestamp*/, void* data, size_t size,
	const media_format& format)
{
	FcRecord* record = static_cast<FcRecord*>(cookie);
	const media_raw_audio_format& raw = format.u.raw_audio;
	uint32 sampleSize = raw.format & media_raw_audio_format::B_AUDIO_SIZE_MASK;
	if (raw.channel_count == 0 || sampleSize == 0 || size > 64 * 1024 * 1024)
		return;
	size_t count = size / sampleSize;
	if (raw.format == media_raw_audio_format::B_AUDIO_FLOAT) {
		record->sink(record->cookie, static_cast<const float*>(data), count,
			(int32_t)raw.channel_count, raw.frame_rate);
		return;
	}
	if (count > record->scratchLength) {
		float* grown = (float*)realloc(record->scratch, count * sizeof(float));
		if (grown == NULL)
			return;
		record->scratch = grown;
		record->scratchLength = count;
	}
	float* out = record->scratch;
	switch (raw.format) {
		case media_raw_audio_format::B_AUDIO_SHORT:
		{
			const int16* in = static_cast<const int16*>(data);
			for (size_t i = 0; i < count; i++)
				out[i] = in[i] / 32768.0f;
			break;
		}
		case media_raw_audio_format::B_AUDIO_INT:
		{
			const int32* in = static_cast<const int32*>(data);
			for (size_t i = 0; i < count; i++)
				out[i] = in[i] / 2147483648.0f;
			break;
		}
		case media_raw_audio_format::B_AUDIO_CHAR:
		{
			const int8* in = static_cast<const int8*>(data);
			for (size_t i = 0; i < count; i++)
				out[i] = in[i] / 128.0f;
			break;
		}
		case media_raw_audio_format::B_AUDIO_UCHAR:
		{
			const uint8* in = static_cast<const uint8*>(data);
			for (size_t i = 0; i < count; i++)
				out[i] = (in[i] - 128) / 128.0f;
			break;
		}
		default:
			return;
	}
	record->sink(record->cookie, out, count, (int32_t)raw.channel_count,
		raw.frame_rate);
}


// Connects a recorder to the system's audio input and starts it. Buffers go
// to the sink, on the recorder's thread, as interleaved floats.
extern "C" void*
fc_record_open(FcRecordSink sink, void* cookie, float* rate,
	int32_t* channels, char* error, size_t errorLength)
{
	FcRecord* record = new(std::nothrow) FcRecord;
	if (record == NULL)
		return NULL;
	record->sink = sink;
	record->cookie = cookie;
	record->scratch = NULL;
	record->scratchLength = 0;
	record->recorder = new(std::nothrow) BMediaRecorder("FilmCraft voice-over",
		B_MEDIA_RAW_AUDIO);
	status_t status = record->recorder != NULL
		? record->recorder->InitCheck() : B_NO_MEMORY;
	if (status == B_OK)
		status = record->recorder->SetHooks(Recorded, NULL, record);
	media_format format;
	format.type = B_MEDIA_RAW_AUDIO;
	format.u.raw_audio = media_raw_audio_format::wildcard;
	if (status == B_OK)
		status = record->recorder->Connect(format);
	if (status == B_OK)
		status = record->recorder->Start();
	if (status != B_OK) {
		Error(error, errorLength, "the audio input couldn't be opened",
			status);
		if (record->recorder != NULL && record->recorder->IsConnected())
			record->recorder->Disconnect();
		delete record->recorder;
		delete record;
		return NULL;
	}
	const media_raw_audio_format& raw = record->recorder->Format().u.raw_audio;
	*rate = raw.frame_rate;
	*channels = (int32_t)raw.channel_count;
	return record;
}


extern "C" void
fc_record_close(void* handle)
{
	FcRecord* record = static_cast<FcRecord*>(handle);
	if (record == NULL)
		return;
	record->recorder->Stop(true);
	record->recorder->Disconnect();
	delete record->recorder;
	free(record->scratch);
	delete record;
}
