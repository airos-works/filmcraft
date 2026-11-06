/*
 * Hardware video decoding through air/OS's Media Kit decoder add-ons (Haiku;
 * bound in src/media_kit.rs). The video engines of the graphics cards and
 * boards air/OS runs on are media add-ons: nvdec (NVIDIA), amduvd (AMD UVD),
 * the RK3588's, the Allwinner and Qualcomm engines and the Raspberry Pi's. The
 * Media Kit gives a format to the first add-on that claims it and never falls
 * back when that one then refuses a stream, so, as airTime does, the add-on is
 * loaded by name and its decoder is set up and fed here; what it refuses or
 * fails on goes to FilmCraft's own decoders (HybridDecoder).
 *
 * A Media Kit decoder pulls its input (Decoder::GetNextChunk) from inside
 * Decode() and takes a failed pull as the end of the stream, flushing its
 * pictures. FilmCraft's decoders are pushed one access unit at a time, so the
 * add-on's decoder runs on a thread of its own here, whose pulls wait for the
 * next access unit: feeding one returns once the decoder has taken it and asks
 * for more, with every picture it gave out on the way kept for the caller.
 */

#include <Directory.h>
#include <Entry.h>
#include <FindDirectory.h>
#include <MediaDefs.h>
#include <MediaFormats.h>
#include <Path.h>
#include <image.h>

#include <DecoderPlugin.h>

#include <condition_variable>
#include <deque>
#include <mutex>
#include <new>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <string>
#include <thread>
#include <vector>


// libavcodec's numbers, with which the Media Kit's 'ffmp' formats are named.
static const int32 kCodecH264 = 27;
static const int32 kCodecHevc = 173;

// Not Haiku colour spaces: 4:2:0 as a luma plane followed by interleaved Cb
// and Cr, bytes_per_row apart, in eight (NV12) or sixteen bits (P010, the
// value in the high bits).
static const color_space kNV12 = (color_space)0x4e563132;
static const color_space kP010 = (color_space)0x50303130;

// Presentation times travel through the add-on in bigtime_t, where some
// add-ons take 0 for "no time": shifted out of the way.
static const int64 kTimeOffset = (int64)1 << 40;


struct AddOnKind {
	const char*	leaf;
	// NewDecoder()'s index for H.264 and HEVC; -1: not this codec.
	int32		h264Index;
	int32		hevcIndex;
	int32		maxWidth;
	int32		maxHeight;
	int32		maxBits;
	// Measured with FilmCraft; the others need FILMCRAFT_HW_DECODERS=all.
	bool		verified;
};


static const AddOnKind kKinds[] = {
	{"nvdec", 0, 1, 4096, 4096, 10, true},
	{"amduvd", 0, 1, 4096, 4096, 10, false},
	{"00_rockchip_mpp", 0, 0, 8192, 4320, 10, false},
	{"00_qcom_video", 1, 1, 4096, 2304, 10, false},
	{"00_sunxi_cedar", 1, 1, 4096, 2304, 10, false},
	{"rpi_hevc", -1, 0, 4096, 4096, 10, false},
	{"rpi_mmal", 0, -1, 1920, 1088, 8, false},
};


struct LoadedAddOn {
	const AddOnKind*	kind;
	image_id			image;
	DecoderPlugin*		plugin;
	std::string			path;
};


static std::mutex sAddOnLock;
static bool sScanned = false;
static std::vector<LoadedAddOn> sAddOns;


static void
ScanAddOns()
{
	// Called with sAddOnLock held.
	if (sScanned)
		return;
	sScanned = true;
	const char* which = getenv("FILMCRAFT_HW_DECODERS");
	bool all = which != NULL && strcmp(which, "all") == 0;
	char** paths = NULL;
	size_t count = 0;
	if (find_paths(B_FIND_PATH_ADD_ONS_DIRECTORY, "media/plugins", &paths,
			&count) != B_OK) {
		return;
	}
	for (const AddOnKind& kind : kKinds) {
		if (!kind.verified && !all)
			continue;
		for (size_t i = 0; i < count; i++) {
			BPath path(paths[i], kind.leaf);
			BEntry entry(path.Path());
			if (!entry.Exists())
				continue;
			image_id image = load_add_on(path.Path());
			if (image < 0)
				continue;
			MediaPlugin* (*instantiate)() = NULL;
			if (get_image_symbol(image, "instantiate_plugin",
					B_SYMBOL_TYPE_TEXT, (void**)&instantiate) != B_OK
				|| instantiate == NULL) {
				unload_add_on(image);
				continue;
			}
			MediaPlugin* plugin = instantiate();
			DecoderPlugin* decoderPlugin = dynamic_cast<DecoderPlugin*>(plugin);
			if (decoderPlugin == NULL) {
				delete plugin;
				unload_add_on(image);
				continue;
			}
			sAddOns.push_back(LoadedAddOn{&kind, image, decoderPlugin,
				path.Path()});
			// The first directory wins, as in the Media Kit.
			break;
		}
	}
	free(paths);
}


struct Chunk {
	std::vector<uint8>	data;
	int64				time;
};


struct Picture {
	uint8*	data;
	size_t	size;
	int64	time;
};


class FcHwDecoder;


// The add-on's Decoder deletes its chunk provider with itself: a go-between.
class Provider : public BPrivate::media::ChunkProvider {
public:
	Provider(FcHwDecoder* decoder) : fDecoder(decoder) {}
	status_t GetNextChunk(const void** chunk, size_t* size,
		media_header* header);

private:
	FcHwDecoder*	fDecoder;
};


class FcHwDecoder {
public:
	FcHwDecoder()
		:
		fDecoder(NULL),
		fBufferSize(0),
		fRowBytes(0),
		fRows(0),
		fPushed(0),
		fTaken(0),
		fWaiting(false),
		fEndOfStream(false),
		fEndTaken(false),
		fDrained(false),
		fRestart(false),
		fFailed(false),
		fQuit(false)
	{
	}

	~FcHwDecoder()
	{
		{
			std::lock_guard<std::mutex> lock(fLock);
			fQuit = true;
		}
		fCondition.notify_all();
		if (fThread.joinable())
			fThread.join();
		delete fDecoder;
		for (Picture& p : fOut)
			free(p.data);
	}

	status_t Init(const LoadedAddOn& addOn, bool hevc, int32 width,
		int32 height, int32 bits, std::string* why)
	{
		media_format_description description;
		description.family = B_MISC_FORMAT_FAMILY;
		description.u.misc.file_format = 'ffmp';
		description.u.misc.codec = hevc ? kCodecHevc : kCodecH264;
		media_format format;
		BMediaFormats formats;
		status_t status = formats.InitCheck();
		if (status == B_OK)
			status = formats.GetFormatFor(description, &format);
		if (status != B_OK) {
			format.Clear();
			format.type = B_MEDIA_ENCODED_VIDEO;
			status = formats.MakeFormatFor(&description, 1, &format);
		}
		if (status != B_OK) {
			*why = "the Media Kit has no format for the codec";
			return status;
		}
		format.type = B_MEDIA_ENCODED_VIDEO;
		format.require_flags = 0;
		format.deny_flags = B_MEDIA_MAUI_UNDEFINED_FLAGS;
		media_raw_video_format& output = format.u.encoded_video.output;
		output.field_rate = 0;
		output.interlace = 1;
		output.first_active = 0;
		output.last_active = height - 1;
		output.orientation = B_VIDEO_TOP_LEFT_RIGHT;
		output.pixel_width_aspect = 1;
		output.pixel_height_aspect = 1;
		output.display.format = B_YCbCr420;
		output.display.line_width = width;
		output.display.line_count = height;
		output.display.bytes_per_row = 0;

		int32 index = hevc ? addOn.kind->hevcIndex : addOn.kind->h264Index;
		fDecoder = addOn.plugin->NewDecoder(index);
		if (fDecoder == NULL) {
			*why = "the add-on made no decoder";
			return B_NO_MEMORY;
		}
		fDecoder->SetChunkProvider(new(std::nothrow) Provider(this));
		media_format encoded = format;
		// The parameter sets come in band, as start codes (Annex B).
		status = fDecoder->Setup(&encoded, NULL, 0);
		if (status != B_OK) {
			*why = std::string(addOn.kind->leaf) + " refused the stream: "
				+ strerror(status);
			return status;
		}
		media_format decoded;
		decoded.type = B_MEDIA_RAW_VIDEO;
		decoded.u.raw_video = media_raw_video_format::wildcard;
		color_space wanted = bits > 8 ? kP010 : kNV12;
		decoded.u.raw_video.display.format = wanted;
		status = fDecoder->NegotiateOutputFormat(&decoded);
		if (status != B_OK || decoded.u.raw_video.display.format != wanted) {
			*why = std::string(addOn.kind->leaf)
				+ " doesn't give pictures as NV12 or P010";
			return status != B_OK ? status : B_MEDIA_BAD_FORMAT;
		}
		fRowBytes = decoded.u.raw_video.display.bytes_per_row;
		fRows = decoded.u.raw_video.display.line_count;
		fBufferSize = fRowBytes * fRows * 3 / 2;
		if (fRowBytes == 0 || fRows == 0 || fBufferSize > 512 * 1024 * 1024) {
			*why = "the add-on gave no picture size";
			return B_MEDIA_BAD_FORMAT;
		}
		fThread = std::thread(&FcHwDecoder::_Run, this);
		return B_OK;
	}

	// Hands over one access unit and waits until the decoder has taken it
	// and asks for the next (or fails).
	status_t Feed(const uint8* data, size_t size, int64 time)
	{
		std::unique_lock<std::mutex> lock(fLock);
		if (fFailed)
			return B_ERROR;
		Chunk chunk;
		chunk.data.assign(data, data + size);
		chunk.time = time + kTimeOffset;
		fIn.push_back(std::move(chunk));
		fPushed++;
		fCondition.notify_all();
		fCondition.wait(lock, [this] {
			return fFailed || (fTaken == fPushed && fWaiting);
		});
		return fFailed ? B_ERROR : B_OK;
	}

	// The end of the stream: every picture the decoder still holds comes
	// out, and it starts over for the next access unit.
	status_t Flush()
	{
		std::unique_lock<std::mutex> lock(fLock);
		if (fFailed)
			return B_ERROR;
		fEndOfStream = true;
		fCondition.notify_all();
		fCondition.wait(lock, [this] { return fFailed || fDrained; });
		if (fFailed)
			return B_ERROR;
		fRestart = true;
		fCondition.notify_all();
		fCondition.wait(lock, [this] {
			return fFailed || (!fRestart && fWaiting);
		});
		return fFailed ? B_ERROR : B_OK;
	}

	// Takes the oldest picture the decoder gave out; its data is the
	// caller's, to free().
	bool Next(Picture* picture)
	{
		std::lock_guard<std::mutex> lock(fLock);
		if (fOut.empty())
			return false;
		*picture = fOut.front();
		fOut.pop_front();
		return true;
	}

	void Discard()
	{
		std::lock_guard<std::mutex> lock(fLock);
		for (Picture& p : fOut)
			free(p.data);
		fOut.clear();
	}

	size_t RowBytes() const { return fRowBytes; }
	size_t Rows() const { return fRows; }

	// On the decoder's thread, from inside Decode().
	status_t NextChunk(const void** chunk, size_t* size, media_header* header)
	{
		std::unique_lock<std::mutex> lock(fLock);
		fCurrent.data.clear();
		// After the end of the stream the add-on gives out what it holds,
		// one picture a Decode(), and asks again: the end stays the answer
		// until _Run() starts it over.
		if (fEndTaken)
			return B_LAST_BUFFER_ERROR;
		fWaiting = true;
		fCondition.notify_all();
		fCondition.wait(lock, [this] {
			return fQuit || !fIn.empty() || fEndOfStream;
		});
		fWaiting = false;
		if (fQuit)
			return B_ERROR;
		if (!fIn.empty()) {
			fCurrent = std::move(fIn.front());
			fIn.pop_front();
			fTaken++;
			*chunk = fCurrent.data.data();
			*size = fCurrent.data.size();
			memset(header, 0, sizeof(*header));
			header->type = B_MEDIA_ENCODED_VIDEO;
			header->start_time = fCurrent.time;
			header->size_used = fCurrent.data.size();
			return B_OK;
		}
		fEndOfStream = false;
		fEndTaken = true;
		return B_LAST_BUFFER_ERROR;
	}

private:
	void _Run()
	{
		for (;;) {
			{
				std::lock_guard<std::mutex> lock(fLock);
				if (fQuit)
					return;
			}
			uint8* buffer = (uint8*)malloc(fBufferSize);
			if (buffer == NULL) {
				_Fail();
				return;
			}
			int64 count = 0;
			media_header header;
			memset(&header, 0, sizeof(header));
			media_decode_info info;
			status_t status = fDecoder->Decode(buffer, &count, &header, &info);
			std::unique_lock<std::mutex> lock(fLock);
			if (fQuit) {
				free(buffer);
				return;
			}
			if (status == B_OK && count > 0) {
				fOut.push_back(Picture{buffer, fBufferSize,
					header.start_time - kTimeOffset});
				continue;
			}
			free(buffer);
			if (status == B_OK)
				continue;
			if (!fEndTaken) {
				// Not the end of the stream: the add-on gave up.
				fFailed = true;
				fCondition.notify_all();
				return;
			}
			fDrained = true;
			fCondition.notify_all();
			fCondition.wait(lock, [this] { return fQuit || fRestart; });
			if (fQuit)
				return;
			lock.unlock();
			fDecoder->SeekedTo(0, 0);
			lock.lock();
			fEndTaken = false;
			fDrained = false;
			fRestart = false;
			fCondition.notify_all();
		}
	}

	void _Fail()
	{
		std::lock_guard<std::mutex> lock(fLock);
		fFailed = true;
		fCondition.notify_all();
	}

	Decoder*				fDecoder;
	size_t					fBufferSize;
	size_t					fRowBytes;
	size_t					fRows;
	std::thread				fThread;
	std::mutex				fLock;
	std::condition_variable	fCondition;
	std::deque<Chunk>		fIn;
	Chunk					fCurrent;
	std::deque<Picture>		fOut;
	uint64					fPushed;
	uint64					fTaken;
	bool					fWaiting;
	bool					fEndOfStream;
	bool					fEndTaken;
	bool					fDrained;
	bool					fRestart;
	bool					fFailed;
	bool					fQuit;
};


status_t
Provider::GetNextChunk(const void** chunk, size_t* size, media_header* header)
{
	return fDecoder->NextChunk(chunk, size, header);
}


static void
SetError(char* error, size_t length, const std::string& text)
{
	if (error != NULL && length > 0)
		snprintf(error, length, "%s", text.c_str());
}


extern "C" {


// The add-ons there are, as "name\n" lines (empty: none).
char*
fc_hwdec_probe()
{
	std::lock_guard<std::mutex> lock(sAddOnLock);
	ScanAddOns();
	std::string names;
	for (const LoadedAddOn& addOn : sAddOns) {
		names += addOn.kind->leaf;
		names += '\n';
	}
	return strdup(names.c_str());
}


void
fc_hwdec_free(void* data)
{
	free(data);
}


// A decoder for an H.264 (hevc 0) or HEVC stream of width x height output
// pictures, from the first add-on that takes it.
void*
fc_hwdec_open(int32_t hevc, int32_t width, int32_t height, int32_t bits,
	char* name, size_t nameLength, char* error, size_t errorLength)
{
	std::lock_guard<std::mutex> lock(sAddOnLock);
	ScanAddOns();
	std::string why = "no hardware decoder add-on";
	for (const LoadedAddOn& addOn : sAddOns) {
		const AddOnKind& kind = *addOn.kind;
		if ((hevc ? kind.hevcIndex : kind.h264Index) < 0
			|| width > kind.maxWidth || height > kind.maxHeight
			|| bits > kind.maxBits || (!hevc && bits > 8)) {
			why = std::string(kind.leaf) + " doesn't decode this stream";
			continue;
		}
		FcHwDecoder* decoder = new(std::nothrow) FcHwDecoder;
		if (decoder == NULL)
			break;
		if (decoder->Init(addOn, hevc != 0, width, height, bits, &why)
				== B_OK) {
			if (name != NULL && nameLength > 0)
				snprintf(name, nameLength, "%s", kind.leaf);
			return decoder;
		}
		delete decoder;
	}
	SetError(error, errorLength, why);
	return NULL;
}


int32_t
fc_hwdec_feed(void* handle, const uint8_t* data, size_t size, int64_t time)
{
	return static_cast<FcHwDecoder*>(handle)->Feed(data, size, time);
}


int32_t
fc_hwdec_flush(void* handle)
{
	return static_cast<FcHwDecoder*>(handle)->Flush();
}


// The next picture: its pixels (malloc'd, the caller frees them with
// fc_hwdec_free), their size and time. 0 when there is none.
int32_t
fc_hwdec_next(void* handle, uint8_t** data, size_t* size, int64_t* time)
{
	Picture picture;
	if (!static_cast<FcHwDecoder*>(handle)->Next(&picture))
		return 0;
	*data = picture.data;
	*size = picture.size;
	*time = picture.time;
	return 1;
}


void
fc_hwdec_discard(void* handle)
{
	static_cast<FcHwDecoder*>(handle)->Discard();
}


void
fc_hwdec_geometry(void* handle, size_t* rowBytes, size_t* rows)
{
	FcHwDecoder* decoder = static_cast<FcHwDecoder*>(handle);
	*rowBytes = decoder->RowBytes();
	*rows = decoder->Rows();
}


void
fc_hwdec_close(void* handle)
{
	delete static_cast<FcHwDecoder*>(handle);
}


}
