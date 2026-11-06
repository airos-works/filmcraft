/*
 * media-fixture: writes a short test movie with the Media Kit's encoders, for
 * trying FilmCraft's Media Kit import (src/media_kit.rs) on air/OS without
 * ffmpeg's command line: a moving colour pattern and a sine tone.
 *
 *   g++ -O2 -o media-fixture media-fixture.cpp -lbe -lmedia
 *   media-fixture list
 *   media-fixture OUT FORMAT VIDEO_CODEC [AUDIO_CODEC] [SECONDS]
 *   media-fixture clip.avi avi mpeg4 mp3 6
 *
 * FORMAT is a file format's short name (avi, asf, …) and the codecs are
 * encoders' short names, as `list` prints them.
 */

#include <Entry.h>
#include <MediaDefs.h>
#include <MediaFile.h>
#include <MediaFormats.h>
#include <MediaTrack.h>

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vector>


static bool
FindFormat(const char* name, media_file_format* out)
{
	int32 cookie = 0;
	while (get_next_file_format(&cookie, out) == B_OK) {
		if (strcmp(out->short_name, name) == 0)
			return true;
	}
	return false;
}


static bool
FindEncoder(const media_file_format& file, const media_format& raw,
	const char* name, media_format* encoded, media_codec_info* info)
{
	int32 cookie = 0;
	while (get_next_encoder(&cookie, &file, &raw, encoded, info) == B_OK) {
		if (strcmp(info->short_name, name) == 0)
			return true;
	}
	return false;
}


static void
List()
{
	media_format video;
	video.type = B_MEDIA_RAW_VIDEO;
	video.u.raw_video = media_raw_video_format::wildcard;
	video.u.raw_video.display.format = B_RGB32;
	media_format audio;
	audio.type = B_MEDIA_RAW_AUDIO;
	audio.u.raw_audio = media_raw_audio_format::wildcard;
	audio.u.raw_audio.format = media_raw_audio_format::B_AUDIO_SHORT;
	int32 cookie = 0;
	media_file_format file;
	while (get_next_file_format(&cookie, &file) == B_OK) {
		printf("format %s (%s):", file.short_name, file.pretty_name);
		const media_format* raws[] = {&video, &audio};
		for (const media_format* raw : raws) {
			int32 codecCookie = 0;
			media_codec_info info;
			media_format out;
			while (get_next_encoder(&codecCookie, &file, raw, &out, &info)
					== B_OK)
				printf(" %s", info.short_name);
			printf(" |");
		}
		printf("\n");
	}
}


int
main(int argc, char** argv)
{
	if (argc == 2 && strcmp(argv[1], "list") == 0) {
		List();
		return 0;
	}
	if (argc < 4) {
		fprintf(stderr, "usage: %s OUT FORMAT VIDEO_CODEC [AUDIO_CODEC] "
			"[SECONDS] | list\n", argv[0]);
		return 2;
	}
	const char* audioName = argc > 4 && strcmp(argv[4], "-") != 0
		? argv[4] : NULL;
	double seconds = argc > 5 ? atof(argv[5]) : 6;
	const int width = 640, height = 360;
	const float fps = 25;
	const float rate = 44100;

	media_file_format file;
	if (!FindFormat(argv[2], &file)) {
		fprintf(stderr, "no file format %s\n", argv[2]);
		return 1;
	}
	entry_ref ref;
	get_ref_for_path(argv[1], &ref);
	BMediaFile media(&ref, &file);
	if (media.InitCheck() != B_OK) {
		fprintf(stderr, "can't create %s: %s\n", argv[1],
			strerror(media.InitCheck()));
		return 1;
	}

	media_format raw;
	raw.type = B_MEDIA_RAW_VIDEO;
	raw.u.raw_video = media_raw_video_format::wildcard;
	raw.u.raw_video.field_rate = fps;
	raw.u.raw_video.interlace = 1;
	raw.u.raw_video.display.format = B_RGB32;
	raw.u.raw_video.display.line_width = width;
	raw.u.raw_video.display.line_count = height;
	raw.u.raw_video.display.bytes_per_row = width * 4;
	raw.u.raw_video.last_active = height - 1;
	raw.u.raw_video.orientation = B_VIDEO_TOP_LEFT_RIGHT;
	raw.u.raw_video.pixel_width_aspect = 1;
	raw.u.raw_video.pixel_height_aspect = 1;
	media_format encoded;
	media_codec_info codec;
	if (!FindEncoder(file, raw, argv[3], &encoded, &codec)) {
		fprintf(stderr, "no video encoder %s for %s\n", argv[3], argv[2]);
		return 1;
	}
	BMediaTrack* video = media.CreateTrack(&raw, &codec);
	if (video == NULL) {
		fprintf(stderr, "can't add the video track\n");
		return 1;
	}

	BMediaTrack* audio = NULL;
	media_format rawAudio;
	if (audioName != NULL) {
		rawAudio.type = B_MEDIA_RAW_AUDIO;
		rawAudio.u.raw_audio = media_raw_audio_format::wildcard;
		rawAudio.u.raw_audio.format = media_raw_audio_format::B_AUDIO_SHORT;
		rawAudio.u.raw_audio.byte_order = B_MEDIA_HOST_ENDIAN;
		rawAudio.u.raw_audio.channel_count = 2;
		rawAudio.u.raw_audio.frame_rate = rate;
		rawAudio.u.raw_audio.buffer_size = 1152 * 4;
		media_format audioEncoded;
		media_codec_info audioCodec;
		if (!FindEncoder(file, rawAudio, audioName, &audioEncoded,
				&audioCodec)) {
			fprintf(stderr, "no audio encoder %s for %s\n", audioName,
				argv[2]);
			return 1;
		}
		audio = media.CreateTrack(&rawAudio, &audioCodec);
		if (audio == NULL) {
			fprintf(stderr, "can't add the audio track\n");
			return 1;
		}
	}
	if (media.CommitHeader() != B_OK) {
		fprintf(stderr, "can't write the header\n");
		return 1;
	}

	int frames = (int)(seconds * fps);
	std::vector<uint32> pixels(width * height);
	int64 audioFrame = 0;
	std::vector<int16> samples;
	for (int f = 0; f < frames; f++) {
		for (int y = 0; y < height; y++) {
			for (int x = 0; x < width; x++) {
				uint8 r = (uint8)((x + f * 4) & 255);
				uint8 g = (uint8)((y * 255) / height);
				uint8 b = (uint8)(((x / 40 + y / 40 + f / 5) & 1) ? 200 : 40);
				pixels[y * width + x] = 0xff000000 | (r << 16) | (g << 8) | b;
			}
		}
		if (video->WriteFrames(pixels.data(), 1,
				f % 25 == 0 ? B_MEDIA_KEY_FRAME : 0) != B_OK) {
			fprintf(stderr, "writing frame %d failed\n", f);
			return 1;
		}
		if (audio != NULL) {
			int64 until = (int64)((f + 1) * rate / fps);
			int count = (int)(until - audioFrame);
			samples.resize(count * 2);
			for (int i = 0; i < count; i++) {
				int16 s = (int16)(8000 * sin(2 * M_PI * 550
					* (audioFrame + i) / rate));
				samples[i * 2] = s;
				samples[i * 2 + 1] = s;
			}
			if (audio->WriteFrames(samples.data(), count) != B_OK) {
				fprintf(stderr, "writing audio failed\n");
				return 1;
			}
			audioFrame = until;
		}
	}
	media.CloseFile();
	printf("%s: %d frames of %s in %s%s%s\n", argv[1], frames,
		codec.pretty_name, file.pretty_name, audio != NULL ? " with " : "",
		audioName != NULL ? audioName : "");
	return 0;
}
