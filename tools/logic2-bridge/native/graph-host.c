#define _GNU_SOURCE

#if defined(_WIN32)
#define WIN32_LEAN_AND_MEAN
#include <winsock2.h>
#include <windows.h>
#include <objbase.h>
#include <fcntl.h>
#include <io.h>
#include <process.h>
#include <wchar.h>
#pragma comment(lib, "Ws2_32.lib")
#pragma comment(lib, "Ole32.lib")
#else
#include <dlfcn.h>
#endif
#include <errno.h>
#if defined(__APPLE__)
#include <libkern/OSCacheControl.h>
#include <mach/mach.h>
#include <mach-o/loader.h>
#elif defined(__linux__)
#include <elf.h>
#endif
#if !defined(_WIN32)
#include <pthread.h>
#endif
#include <signal.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#if !defined(_WIN32)
#include <strings.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#endif

#if defined(_WIN32)
#define bridge_strcasecmp _stricmp
#define BRIDGE_NOINLINE __declspec(noinline)
#else
#define bridge_strcasecmp strcasecmp
#define BRIDGE_NOINLINE __attribute__((noinline))
#endif

typedef void *(*CreateGraphServerFn)(uint32_t *, uint32_t, bool, bool, const char *);
typedef void (*DestroyGraphServerFn)(void *);
typedef void (*SetLogFileNameFn)(const char *);
typedef bool (*FlushLogFn)(void);

typedef struct {
  void *data;
  void *shared_owner;
  uint64_t size;
} SaleaeBuffer;

#if defined(_WIN32) && defined(_M_X64)
// Microsoft x64 passes both non-trivial aggregates indirectly: RDX=DeviceId,
// R8=Buffer. This layout is verified against the 2.4.46 PE function body.
typedef void (*OnDataBufferFn)(void *, void *, SaleaeBuffer *);
typedef CRITICAL_SECTION BridgeMutex;
typedef CONDITION_VARIABLE BridgeCondition;
#else
// AArch64/SysV pass DeviceId in two integer slots, followed by an indirect Buffer.
typedef void (*OnDataBufferFn)(void *, uint64_t, uint64_t, SaleaeBuffer *);
typedef pthread_mutex_t BridgeMutex;
typedef pthread_cond_t BridgeCondition;
#endif

typedef struct {
  uint8_t *data;
  size_t capacity;
  size_t read_offset;
  size_t size;
  BridgeMutex mutex;
  BridgeCondition data_available;
} ByteRing;

static volatile sig_atomic_t stop_requested = 0;
static OnDataBufferFn original_on_data_buffer = NULL;
static ByteRing injection_ring = {0};
static _Atomic bool producer_eof = false;
static _Atomic bool capture_input_ended = true;
static _Atomic size_t injection_stripe_bytes = 32;
static _Atomic uint64_t injected_callback_count = 0;
static _Atomic uint64_t injected_byte_count = 0;
static _Atomic uint64_t dropped_byte_count = 0;
static _Atomic uint64_t underflow_count = 0;

enum {
  HOOK_MAX_PROLOGUE_SIZE = 64,
};

typedef struct {
  const char *profile_id;
  const char *graph_identity;
  size_t on_data_buffer_offset;
  uint8_t prologue[HOOK_MAX_PROLOGUE_SIZE];
  size_t prologue_size;
} HookConfiguration;

static HookConfiguration hook_configuration = {0};

enum {
  INJECTION_RING_CAPACITY = 128u * 1024u * 1024u,
  INJECTION_WAIT_MILLISECONDS = 10000,
  INJECTION_FRAME_HEADER_BYTES = 12,
  INJECTION_FRAME_MAX_PAYLOAD = 64u * 1024u * 1024u,
  INJECTION_FRAME_CONFIG = 1,
  INJECTION_FRAME_DATA = 2,
  INJECTION_FRAME_END = 3,
};

#if defined(__APPLE__) && defined(__aarch64__)
enum {
  ABSOLUTE_BRANCH_SIZE = 16,
};
#elif defined(__linux__) && defined(__x86_64__)
enum {
  ABSOLUTE_BRANCH_SIZE = 12,
};
#elif defined(_WIN32) && defined(_M_X64)
enum {
  // ff 25 00 00 00 00 + 64-bit target: an indirect RIP-relative jump which
  // preserves RAX. The verified OnDataBuffer prologue leaves RAX holding RSP
  // for the function body, so the 12-byte movabs/jmp sequence is invalid here.
  ABSOLUTE_BRANCH_SIZE = 14,
};
#else
enum {
  ABSOLUTE_BRANCH_SIZE = 0,
};
#endif

static void handle_signal(int signal_number) {
  (void)signal_number;
  stop_requested = 1;
}

static void bridge_mutex_lock(BridgeMutex *mutex) {
#if defined(_WIN32)
  EnterCriticalSection(mutex);
#else
  (void)pthread_mutex_lock(mutex);
#endif
}

static void bridge_mutex_unlock(BridgeMutex *mutex) {
#if defined(_WIN32)
  LeaveCriticalSection(mutex);
#else
  (void)pthread_mutex_unlock(mutex);
#endif
}

static void bridge_condition_broadcast(BridgeCondition *condition) {
#if defined(_WIN32)
  WakeAllConditionVariable(condition);
#else
  (void)pthread_cond_broadcast(condition);
#endif
}

static int hex_digit(char value) {
  if (value >= '0' && value <= '9') return value - '0';
  if (value >= 'a' && value <= 'f') return value - 'a' + 10;
  if (value >= 'A' && value <= 'F') return value - 'A' + 10;
  return -1;
}

static bool configure_hook(const char *profile_id, const char *graph_identity,
                           const char *offset_text, const char *prologue_hex) {
  char *end = NULL;
  errno = 0;
  unsigned long long offset = strtoull(offset_text, &end, 0);
  if (errno != 0 || end == offset_text || *end != '\0' || offset > SIZE_MAX) {
    fprintf(stderr, "[logic2-bridge:inject] invalid hook offset: %s\n", offset_text);
    return false;
  }

  size_t hex_length = strlen(prologue_hex);
  size_t prologue_size = hex_length / 2;
  if (hex_length == 0 || hex_length % 2 != 0 ||
      prologue_size < ABSOLUTE_BRANCH_SIZE ||
      prologue_size > HOOK_MAX_PROLOGUE_SIZE) {
    fprintf(stderr, "[logic2-bridge:inject] invalid hook prologue length: %zu\n",
            hex_length);
    return false;
  }
  for (size_t index = 0; index < prologue_size; index += 1) {
    int high = hex_digit(prologue_hex[index * 2]);
    int low = hex_digit(prologue_hex[index * 2 + 1]);
    if (high < 0 || low < 0) {
      fprintf(stderr, "[logic2-bridge:inject] hook prologue is not hexadecimal\n");
      return false;
    }
    hook_configuration.prologue[index] = (uint8_t)((high << 4) | low);
  }

  hook_configuration.profile_id = profile_id;
  hook_configuration.graph_identity = graph_identity;
  hook_configuration.on_data_buffer_offset = (size_t)offset;
  hook_configuration.prologue_size = prologue_size;
  return true;
}

#if defined(__APPLE__) && defined(__aarch64__)
static void format_uuid(const uint8_t uuid[16], char output[37]) {
  (void)snprintf(output, 37,
                 "%02X%02X%02X%02X-%02X%02X-%02X%02X-"
                 "%02X%02X-%02X%02X%02X%02X%02X%02X",
                 uuid[0], uuid[1], uuid[2], uuid[3], uuid[4], uuid[5],
                 uuid[6], uuid[7], uuid[8], uuid[9], uuid[10], uuid[11],
                 uuid[12], uuid[13], uuid[14], uuid[15]);
}

static bool verify_graph_uuid(const void *image_base) {
  const struct mach_header_64 *header = (const struct mach_header_64 *)image_base;
  if (header == NULL || header->magic != MH_MAGIC_64) {
    fprintf(stderr, "[logic2-bridge:inject] GraphServer is not a 64-bit Mach-O image\n");
    return false;
  }

  const uint8_t *command_data = (const uint8_t *)(header + 1);
  size_t remaining = header->sizeofcmds;
  for (uint32_t index = 0; index < header->ncmds; index += 1) {
    if (remaining < sizeof(struct load_command)) break;
    const struct load_command *command = (const struct load_command *)command_data;
    if (command->cmdsize < sizeof(struct load_command) || command->cmdsize > remaining) break;
    if (command->cmd == LC_UUID && command->cmdsize >= sizeof(struct uuid_command)) {
      const struct uuid_command *uuid_command = (const struct uuid_command *)command;
      char actual[37];
      format_uuid(uuid_command->uuid, actual);
      if (bridge_strcasecmp(actual, hook_configuration.graph_identity) != 0) {
        fprintf(stderr,
                "[logic2-bridge:inject] GraphServer identity mismatch: "
                "expected %s, found %s\n",
                hook_configuration.graph_identity, actual);
        return false;
      }
      fprintf(stderr,
              "[logic2-bridge:inject] verified GraphServer profile %s (%s)\n",
              hook_configuration.profile_id, actual);
      return true;
    }
    command_data += command->cmdsize;
    remaining -= command->cmdsize;
  }
  fprintf(stderr, "[logic2-bridge:inject] GraphServer LC_UUID was not found\n");
  return false;
}
#endif

#if defined(__linux__) && defined(__x86_64__)
static bool verify_graph_build_id(const void *image_base) {
  const Elf64_Ehdr *header = (const Elf64_Ehdr *)image_base;
  if (header == NULL || memcmp(header->e_ident, ELFMAG, SELFMAG) != 0 ||
      header->e_ident[EI_CLASS] != ELFCLASS64 ||
      header->e_ident[EI_DATA] != ELFDATA2LSB) {
    fprintf(stderr, "[logic2-bridge:inject] GraphServer is not a little-endian ELF64 image\n");
    return false;
  }

  const uint8_t *base = (const uint8_t *)image_base;
  const Elf64_Phdr *programs = (const Elf64_Phdr *)(base + header->e_phoff);
  for (Elf64_Half index = 0; index < header->e_phnum; index += 1) {
    const Elf64_Phdr *program = &programs[index];
    if (program->p_type != PT_NOTE) continue;
    const uint8_t *note = base + program->p_vaddr;
    const uint8_t *end = note + program->p_memsz;
    while ((size_t)(end - note) >= sizeof(Elf64_Nhdr)) {
      const Elf64_Nhdr *header_note = (const Elf64_Nhdr *)note;
      const uint8_t *name = note + sizeof(Elf64_Nhdr);
      const uint8_t *description = name + ((header_note->n_namesz + 3u) & ~3u);
      const uint8_t *next = description + ((header_note->n_descsz + 3u) & ~3u);
      if (next > end) break;
      if (header_note->n_type == NT_GNU_BUILD_ID && header_note->n_namesz >= 3 &&
          memcmp(name, "GNU", 3) == 0) {
        if (header_note->n_descsz == 0 || header_note->n_descsz > 64) {
          fprintf(stderr, "[logic2-bridge:inject] invalid ELF Build ID length: %u\n",
                  header_note->n_descsz);
          return false;
        }
        char actual[129];
        for (size_t byte = 0; byte < header_note->n_descsz; byte += 1) {
          (void)snprintf(actual + byte * 2, 3, "%02x", description[byte]);
        }
        actual[header_note->n_descsz * 2] = '\0';
        if (bridge_strcasecmp(actual, hook_configuration.graph_identity) != 0) {
          fprintf(stderr,
                  "[logic2-bridge:inject] GraphServer identity mismatch: "
                  "expected %s, found %s\n",
                  hook_configuration.graph_identity, actual);
          return false;
        }
        fprintf(stderr,
                "[logic2-bridge:inject] verified GraphServer profile %s (%s)\n",
                hook_configuration.profile_id, actual);
        return true;
      }
      note = next;
    }
  }
  fprintf(stderr, "[logic2-bridge:inject] GraphServer ELF Build ID was not found\n");
  return false;
}
#endif

#if defined(_WIN32) && defined(_M_X64)
static bool verify_graph_codeview_identity(const void *image_base) {
  const uint8_t *base = (const uint8_t *)image_base;
  const IMAGE_DOS_HEADER *dos = (const IMAGE_DOS_HEADER *)base;
  if (dos == NULL || dos->e_magic != IMAGE_DOS_SIGNATURE || dos->e_lfanew <= 0) {
    fprintf(stderr, "[logic2-bridge:inject] GraphServer is not a PE image\n");
    return false;
  }

  const IMAGE_NT_HEADERS64 *nt =
      (const IMAGE_NT_HEADERS64 *)(base + (size_t)dos->e_lfanew);
  if (nt->Signature != IMAGE_NT_SIGNATURE ||
      nt->OptionalHeader.Magic != IMAGE_NT_OPTIONAL_HDR64_MAGIC ||
      nt->FileHeader.Machine != IMAGE_FILE_MACHINE_AMD64) {
    fprintf(stderr, "[logic2-bridge:inject] GraphServer is not an x64 PE image\n");
    return false;
  }

  const size_t image_size = nt->OptionalHeader.SizeOfImage;
  const IMAGE_DATA_DIRECTORY directory =
      nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_DEBUG];
  if (directory.VirtualAddress == 0 || directory.Size < sizeof(IMAGE_DEBUG_DIRECTORY) ||
      (size_t)directory.VirtualAddress + directory.Size > image_size) {
    fprintf(stderr, "[logic2-bridge:inject] GraphServer PE debug directory was not found\n");
    return false;
  }

  const IMAGE_DEBUG_DIRECTORY *entries =
      (const IMAGE_DEBUG_DIRECTORY *)(base + directory.VirtualAddress);
  const size_t entry_count = directory.Size / sizeof(*entries);
  for (size_t index = 0; index < entry_count; index += 1) {
    const IMAGE_DEBUG_DIRECTORY *entry = &entries[index];
    if (entry->Type != IMAGE_DEBUG_TYPE_CODEVIEW || entry->SizeOfData < 24 ||
        entry->AddressOfRawData == 0 ||
        (size_t)entry->AddressOfRawData + entry->SizeOfData > image_size) {
      continue;
    }
    const uint8_t *codeview = base + entry->AddressOfRawData;
    if (memcmp(codeview, "RSDS", 4) != 0) continue;

    GUID guid;
    uint32_t age;
    memcpy(&guid, codeview + 4, sizeof(guid));
    memcpy(&age, codeview + 20, sizeof(age));
    char actual[48];
    (void)snprintf(actual, sizeof(actual),
                   "%08lX-%04X-%04X-%02X%02X-%02X%02X%02X%02X%02X%02X-%lu",
                   (unsigned long)guid.Data1, guid.Data2, guid.Data3,
                   guid.Data4[0], guid.Data4[1], guid.Data4[2], guid.Data4[3],
                   guid.Data4[4], guid.Data4[5], guid.Data4[6], guid.Data4[7],
                   (unsigned long)age);
    if (bridge_strcasecmp(actual, hook_configuration.graph_identity) != 0) {
      fprintf(stderr,
              "[logic2-bridge:inject] GraphServer identity mismatch: "
              "expected %s, found %s\n",
              hook_configuration.graph_identity, actual);
      return false;
    }
    fprintf(stderr,
            "[logic2-bridge:inject] verified GraphServer profile %s (%s)\n",
            hook_configuration.profile_id, actual);
    return true;
  }

  fprintf(stderr, "[logic2-bridge:inject] GraphServer CodeView identity was not found\n");
  return false;
}
#endif

static bool initialize_injection_ring(void) {
  injection_ring.data = malloc(INJECTION_RING_CAPACITY);
  if (injection_ring.data == NULL) {
    perror("[logic2-bridge:inject] malloc ring");
    return false;
  }
  injection_ring.capacity = INJECTION_RING_CAPACITY;
#if defined(_WIN32)
  InitializeCriticalSection(&injection_ring.mutex);
  InitializeConditionVariable(&injection_ring.data_available);
#else
  if (pthread_mutex_init(&injection_ring.mutex, NULL) != 0 ||
      pthread_cond_init(&injection_ring.data_available, NULL) != 0) {
    fprintf(stderr, "[logic2-bridge:inject] could not initialize ring synchronization\n");
    free(injection_ring.data);
    injection_ring = (ByteRing){0};
    return false;
  }
#endif
  return true;
}

static void configure_injection_ring(size_t stripe_bytes) {
  bridge_mutex_lock(&injection_ring.mutex);
  injection_ring.read_offset = 0;
  injection_ring.size = 0;
  atomic_store_explicit(&injection_stripe_bytes, stripe_bytes, memory_order_release);
  atomic_store_explicit(&capture_input_ended, false, memory_order_release);
  atomic_store_explicit(&injected_callback_count, 0, memory_order_relaxed);
  atomic_store_explicit(&injected_byte_count, 0, memory_order_relaxed);
  atomic_store_explicit(&dropped_byte_count, 0, memory_order_relaxed);
  atomic_store_explicit(&underflow_count, 0, memory_order_relaxed);
  bridge_condition_broadcast(&injection_ring.data_available);
  bridge_mutex_unlock(&injection_ring.mutex);
  fprintf(stderr, "[logic2-bridge:inject] configured stripe=%zu bytes\n", stripe_bytes);
}

static void end_injection_capture(void) {
  atomic_store_explicit(&capture_input_ended, true, memory_order_release);
  bridge_mutex_lock(&injection_ring.mutex);
  bridge_condition_broadcast(&injection_ring.data_available);
  bridge_mutex_unlock(&injection_ring.mutex);
}

static void write_injection_ring(const uint8_t *source, size_t length) {
  if (length == 0) return;
  bridge_mutex_lock(&injection_ring.mutex);

  size_t stripe_bytes = atomic_load_explicit(&injection_stripe_bytes, memory_order_acquire);
  size_t aligned_length = length / stripe_bytes * stripe_bytes;
  if (aligned_length != length) {
    fprintf(stderr, "[logic2-bridge:inject] dropping %zu non-aligned bytes\n",
            length - aligned_length);
    atomic_fetch_add_explicit(&dropped_byte_count, length - aligned_length,
                              memory_order_relaxed);
    length = aligned_length;
  }
  if (length == 0) {
    bridge_mutex_unlock(&injection_ring.mutex);
    return;
  }

  if (length >= injection_ring.capacity) {
    size_t retained = injection_ring.capacity / stripe_bytes * stripe_bytes;
    uint64_t dropped = (uint64_t)injection_ring.size + (uint64_t)(length - retained);
    atomic_fetch_add_explicit(&dropped_byte_count, dropped, memory_order_relaxed);
    source += length - retained;
    length = retained;
    injection_ring.read_offset = 0;
    injection_ring.size = 0;
  } else if (length > injection_ring.capacity - injection_ring.size) {
    size_t dropped = length - (injection_ring.capacity - injection_ring.size);
    dropped = (dropped + stripe_bytes - 1) / stripe_bytes * stripe_bytes;
    injection_ring.read_offset =
        (injection_ring.read_offset + dropped) % injection_ring.capacity;
    injection_ring.size -= dropped;
    atomic_fetch_add_explicit(&dropped_byte_count, dropped, memory_order_relaxed);
  }

  size_t write_offset =
      (injection_ring.read_offset + injection_ring.size) % injection_ring.capacity;
  size_t first = length;
  if (first > injection_ring.capacity - write_offset) {
    first = injection_ring.capacity - write_offset;
  }
  memcpy(injection_ring.data + write_offset, source, first);
  memcpy(injection_ring.data, source + first, length - first);
  injection_ring.size += length;
  bridge_condition_broadcast(&injection_ring.data_available);
  bridge_mutex_unlock(&injection_ring.mutex);
}

static size_t read_injection_ring(uint8_t *destination, size_t length) {
#if !defined(_WIN32)
  struct timespec deadline;
  clock_gettime(CLOCK_REALTIME, &deadline);
  deadline.tv_sec += INJECTION_WAIT_MILLISECONDS / 1000;
  deadline.tv_nsec += (INJECTION_WAIT_MILLISECONDS % 1000) * 1000000L;
  if (deadline.tv_nsec >= 1000000000L) {
    deadline.tv_sec += 1;
    deadline.tv_nsec -= 1000000000L;
  }
#endif

  bridge_mutex_lock(&injection_ring.mutex);
  while (injection_ring.size < length &&
         !atomic_load_explicit(&producer_eof, memory_order_acquire) &&
         !atomic_load_explicit(&capture_input_ended, memory_order_acquire) &&
         !stop_requested) {
#if defined(_WIN32)
    if (!SleepConditionVariableCS(&injection_ring.data_available,
                                  &injection_ring.mutex,
                                  INJECTION_WAIT_MILLISECONDS) &&
        GetLastError() == ERROR_TIMEOUT) {
      break;
    }
#else
    if (pthread_cond_timedwait(&injection_ring.data_available,
                               &injection_ring.mutex, &deadline) != 0) {
      break;
    }
#endif
  }

  size_t copied = length < injection_ring.size ? length : injection_ring.size;
  size_t stripe_bytes = atomic_load_explicit(&injection_stripe_bytes, memory_order_acquire);
  copied = copied / stripe_bytes * stripe_bytes;
  size_t first = copied;
  if (first > injection_ring.capacity - injection_ring.read_offset) {
    first = injection_ring.capacity - injection_ring.read_offset;
  }
  memcpy(destination, injection_ring.data + injection_ring.read_offset, first);
  memcpy(destination + first, injection_ring.data, copied - first);
  injection_ring.read_offset =
      (injection_ring.read_offset + copied) % injection_ring.capacity;
  injection_ring.size -= copied;
  bridge_mutex_unlock(&injection_ring.mutex);
  return copied;
}

static size_t injection_ring_size(void) {
  bridge_mutex_lock(&injection_ring.mutex);
  size_t size = injection_ring.size;
  bridge_mutex_unlock(&injection_ring.mutex);
  return size;
}

static bool read_injection_bytes(uint8_t *destination, size_t length, bool *clean_eof) {
  size_t offset = 0;
  *clean_eof = false;
  while (offset < length) {
#if defined(_WIN32)
    int count = _read(_fileno(stdin), destination + offset,
                      (unsigned int)(length - offset));
#else
    ssize_t count = read(STDIN_FILENO, destination + offset, length - offset);
#endif
    if (count > 0) {
      offset += (size_t)count;
      continue;
    }
    if (count < 0 && errno == EINTR) continue;
    if (count < 0) perror("[logic2-bridge:inject] read stdin");
    if (count == 0 && offset == 0) *clean_eof = true;
    return false;
  }
  return true;
}

static uint32_t read_u32_le(const uint8_t *data) {
  return (uint32_t)data[0] | ((uint32_t)data[1] << 8) |
         ((uint32_t)data[2] << 16) | ((uint32_t)data[3] << 24);
}

#if defined(_WIN32)
static unsigned __stdcall read_injection_stdin(void *unused) {
#else
static void *read_injection_stdin(void *unused) {
#endif
  (void)unused;
  uint8_t header[INJECTION_FRAME_HEADER_BYTES];
  uint8_t *payload = NULL;
  size_t payload_capacity = 0;

  for (;;) {
    bool clean_eof = false;
    if (!read_injection_bytes(header, sizeof(header), &clean_eof)) {
      if (!clean_eof) fprintf(stderr, "[logic2-bridge:inject] truncated frame header\n");
      break;
    }
    if (memcmp(header, "PXLI", 4) != 0 || header[5] != 0 ||
        header[6] != 0 || header[7] != 0) {
      fprintf(stderr, "[logic2-bridge:inject] invalid frame header\n");
      break;
    }

    uint8_t frame_type = header[4];
    uint32_t payload_length = read_u32_le(header + 8);
    if (payload_length > INJECTION_FRAME_MAX_PAYLOAD) {
      fprintf(stderr, "[logic2-bridge:inject] frame is too large: %u\n", payload_length);
      break;
    }
    if (payload_length > payload_capacity) {
      uint8_t *replacement = realloc(payload, payload_length);
      if (replacement == NULL) {
        perror("[logic2-bridge:inject] realloc payload");
        break;
      }
      payload = replacement;
      payload_capacity = payload_length;
    }
    if (payload_length > 0 &&
        !read_injection_bytes(payload, payload_length, &clean_eof)) {
      fprintf(stderr, "[logic2-bridge:inject] truncated frame payload\n");
      break;
    }

    if (frame_type == INJECTION_FRAME_CONFIG && payload_length == 4) {
      size_t stripe_bytes = read_u32_le(payload);
      if (stripe_bytes < 8 || stripe_bytes > 128 || stripe_bytes % 8 != 0) {
        fprintf(stderr, "[logic2-bridge:inject] invalid stripe size: %zu\n", stripe_bytes);
        break;
      }
      configure_injection_ring(stripe_bytes);
    } else if (frame_type == INJECTION_FRAME_DATA) {
      write_injection_ring(payload, payload_length);
    } else if (frame_type == INJECTION_FRAME_END && payload_length == 0) {
      end_injection_capture();
    } else {
      fprintf(stderr, "[logic2-bridge:inject] invalid frame type=%u length=%u\n",
              frame_type, payload_length);
      break;
    }
  }
  free(payload);
  atomic_store_explicit(&producer_eof, true, memory_order_release);
  end_injection_capture();
#if defined(_WIN32)
  return 0;
#else
  return NULL;
#endif
}

static bool start_injection_stdin_reader(void) {
#if defined(_WIN32)
  uintptr_t thread = _beginthreadex(NULL, 0, read_injection_stdin, NULL, 0, NULL);
  if (thread == 0) {
    perror("[logic2-bridge:inject] could not start stdin reader");
    return false;
  }
  CloseHandle((HANDLE)thread);
  return true;
#else
  pthread_t thread;
  int result = pthread_create(&thread, NULL, read_injection_stdin, NULL);
  if (result != 0) {
    fprintf(stderr, "[logic2-bridge:inject] could not start stdin reader: %s\n",
            strerror(result));
    return false;
  }
  (void)pthread_detach(thread);
  return true;
#endif
}

static void *load_symbol(void *library, const char *name) {
#if defined(_WIN32)
  FARPROC symbol = GetProcAddress((HMODULE)library, name);
  if (symbol == NULL) {
    fprintf(stderr, "[logic2-bridge:native] missing %s (error %lu)\n",
            name, (unsigned long)GetLastError());
    exit(1);
  }
  return (void *)symbol;
#else
  dlerror();
  void *symbol = dlsym(library, name);
  const char *error = dlerror();
  if (error != NULL) {
    fprintf(stderr, "[logic2-bridge:native] missing %s: %s\n", name, error);
    exit(1);
  }
  return symbol;
#endif
}

#if defined(__APPLE__) && defined(__aarch64__)
static void write_absolute_branch(uint8_t *destination, const void *target) {
  const uint32_t load_target = 0x58000050;  // ldr x16, #8
  const uint32_t branch_target = 0xd61f0200;  // br x16
  memcpy(destination, &load_target, sizeof(load_target));
  memcpy(destination + 4, &branch_target, sizeof(branch_target));
  memcpy(destination + 8, &target, sizeof(target));
}

static bool set_code_page_protection(void *address, size_t length, vm_prot_t protection) {
  const vm_size_t page_size = (vm_size_t)getpagesize();
  const vm_address_t page = (vm_address_t)address & ~(page_size - 1);
  const vm_address_t end =
      ((vm_address_t)address + length + page_size - 1) & ~(page_size - 1);
  return vm_protect(mach_task_self(), page, end - page, false, protection) == KERN_SUCCESS;
}
#elif defined(__linux__) && defined(__x86_64__)
static void write_absolute_branch(uint8_t *destination, const void *target) {
  const uint64_t address = (uint64_t)(uintptr_t)target;
  destination[0] = 0x48;  // movabs rax, imm64
  destination[1] = 0xb8;
  memcpy(destination + 2, &address, sizeof(address));
  destination[10] = 0xff;  // jmp rax
  destination[11] = 0xe0;
}
#elif defined(_WIN32) && defined(_M_X64)
static void write_absolute_branch(uint8_t *destination, const void *target) {
  const uint64_t address = (uint64_t)(uintptr_t)target;
  // jmp qword ptr [rip+0], followed by its absolute destination. Unlike the
  // Linux movabs sequence, this preserves the RAX value established by the
  // copied MSVC function prologue in the trampoline.
  destination[0] = 0xff;
  destination[1] = 0x25;
  destination[2] = 0x00;
  destination[3] = 0x00;
  destination[4] = 0x00;
  destination[5] = 0x00;
  memcpy(destination + 6, &address, sizeof(address));
}

#endif

#if defined(_WIN32)
static bool set_code_page_protection(void *address, size_t length,
                                     DWORD protection, DWORD *previous) {
  return VirtualProtect(address, length, protection, previous) != 0;
}
#elif defined(__linux__)
static bool set_code_page_protection(void *address, size_t length, int protection) {
  const size_t page_size = (size_t)getpagesize();
  const uintptr_t begin = (uintptr_t)address & ~(page_size - 1u);
  const uintptr_t end = ((uintptr_t)address + length + page_size - 1u) & ~(page_size - 1u);
  return mprotect((void *)begin, end - begin, protection) == 0;
}
#endif

static void inject_capture_buffer(SaleaeBuffer *buffer) {
  if (buffer != NULL && buffer->data != NULL && buffer->size > 0 &&
      buffer->size <= 64u * 1024u * 1024u) {
    size_t stripe_bytes = atomic_load_explicit(&injection_stripe_bytes, memory_order_acquire);
    if (buffer->size % stripe_bytes != 0) {
      fprintf(stderr,
              "[logic2-bridge:inject] callback buffer=%llu is not aligned to stripe=%zu\n",
              (unsigned long long)buffer->size, stripe_bytes);
    }
    size_t injected = read_injection_ring(buffer->data, (size_t)buffer->size);
    if (injected < buffer->size) {
      memset((uint8_t *)buffer->data + injected, 0, (size_t)buffer->size - injected);
      atomic_fetch_add_explicit(&underflow_count, 1, memory_order_relaxed);
    }

    uint64_t callback = atomic_fetch_add_explicit(
                            &injected_callback_count, 1, memory_order_relaxed) + 1;
    uint64_t total = atomic_fetch_add_explicit(
                         &injected_byte_count, injected, memory_order_relaxed) + injected;
    uint64_t underflows = atomic_load_explicit(&underflow_count, memory_order_relaxed);
    bool report_underflow = injected < buffer->size &&
                            (underflows <= 3 || underflows % 128 == 0);
    if (callback <= 3 || callback % 128 == 0 || report_underflow) {
      fprintf(stderr,
              "[logic2-bridge:inject] callback=%llu buffer=%llu injected=%llu "
              "queued=%llu total=%llu underflows=%llu dropped=%llu\n",
              (unsigned long long)callback,
              (unsigned long long)buffer->size,
              (unsigned long long)injected,
              (unsigned long long)injection_ring_size(),
              (unsigned long long)total,
              (unsigned long long)underflows,
              (unsigned long long)atomic_load_explicit(
                  &dropped_byte_count, memory_order_relaxed));
    }
  }
}

#if defined(_WIN32) && defined(_M_X64)
BRIDGE_NOINLINE static void inject_capture_data(
    void *node, void *device_id, SaleaeBuffer *buffer) {
  inject_capture_buffer(buffer);
  original_on_data_buffer(node, device_id, buffer);
}
#else
BRIDGE_NOINLINE static void inject_capture_data(
    void *node, uint64_t device_id_low, uint64_t device_id_high,
    SaleaeBuffer *buffer) {
  inject_capture_buffer(buffer);
  original_on_data_buffer(node, device_id_low, device_id_high, buffer);
}
#endif

static bool install_injection_hook(void *exported_graph_symbol) {
#if defined(__APPLE__) && defined(__aarch64__)
  Dl_info image_info;
  if (dladdr(exported_graph_symbol, &image_info) == 0 || image_info.dli_fbase == NULL) {
    fprintf(stderr, "[logic2-bridge:inject] could not locate GraphServer image base\n");
    return false;
  }
  if (!verify_graph_uuid(image_info.dli_fbase)) return false;

  uint8_t *target =
      (uint8_t *)image_info.dli_fbase + hook_configuration.on_data_buffer_offset;
  if (memcmp(target, hook_configuration.prologue,
             hook_configuration.prologue_size) != 0) {
    fprintf(stderr,
            "[logic2-bridge:inject] unsupported GraphServer: OnDataBuffer "
            "prologue mismatch at +0x%llx\n",
            (unsigned long long)hook_configuration.on_data_buffer_offset);
    return false;
  }

  const size_t trampoline_size =
      hook_configuration.prologue_size + ABSOLUTE_BRANCH_SIZE;
  uint8_t *trampoline = mmap(NULL, trampoline_size, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANON, -1, 0);
  if (trampoline == MAP_FAILED) {
    perror("[logic2-bridge:inject] mmap trampoline");
    return false;
  }
  memcpy(trampoline, target, hook_configuration.prologue_size);
  write_absolute_branch(trampoline + hook_configuration.prologue_size,
                        target + hook_configuration.prologue_size);
  sys_icache_invalidate(trampoline, trampoline_size);
  if (mprotect(trampoline, trampoline_size, PROT_READ | PROT_EXEC) != 0) {
    perror("[logic2-bridge:inject] mprotect trampoline");
    munmap(trampoline, trampoline_size);
    return false;
  }
  original_on_data_buffer = (OnDataBufferFn)trampoline;

  if (!set_code_page_protection(target, hook_configuration.prologue_size,
                                VM_PROT_READ | VM_PROT_WRITE | VM_PROT_COPY)) {
    fprintf(stderr, "[logic2-bridge:inject] could not make GraphServer code writable\n");
    munmap(trampoline, trampoline_size);
    original_on_data_buffer = NULL;
    return false;
  }
  write_absolute_branch(target, (const void *)&inject_capture_data);
  sys_icache_invalidate(target, hook_configuration.prologue_size);
  if (!set_code_page_protection(target, hook_configuration.prologue_size,
                                VM_PROT_READ | VM_PROT_EXECUTE)) {
    fprintf(stderr, "[logic2-bridge:inject] warning: could not restore code protection\n");
  }
  fprintf(stderr, "[logic2-bridge:inject] installed profile %s PXLogic hook at %p\n",
          hook_configuration.profile_id, (void *)target);
  return true;
#elif defined(__linux__) && defined(__x86_64__)
  Dl_info image_info;
  if (dladdr(exported_graph_symbol, &image_info) == 0 || image_info.dli_fbase == NULL) {
    fprintf(stderr, "[logic2-bridge:inject] could not locate GraphServer image base\n");
    return false;
  }
  if (!verify_graph_build_id(image_info.dli_fbase)) return false;

  uint8_t *target =
      (uint8_t *)image_info.dli_fbase + hook_configuration.on_data_buffer_offset;
  if (memcmp(target, hook_configuration.prologue,
             hook_configuration.prologue_size) != 0) {
    fprintf(stderr,
            "[logic2-bridge:inject] unsupported GraphServer: OnDataBuffer "
            "prologue mismatch at +0x%llx\n",
            (unsigned long long)hook_configuration.on_data_buffer_offset);
    return false;
  }

  const size_t trampoline_size =
      hook_configuration.prologue_size + ABSOLUTE_BRANCH_SIZE;
  uint8_t *trampoline = mmap(NULL, trampoline_size, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (trampoline == MAP_FAILED) {
    perror("[logic2-bridge:inject] mmap trampoline");
    return false;
  }
  memcpy(trampoline, target, hook_configuration.prologue_size);
  write_absolute_branch(trampoline + hook_configuration.prologue_size,
                        target + hook_configuration.prologue_size);
  __builtin___clear_cache((char *)trampoline, (char *)trampoline + trampoline_size);
  if (mprotect(trampoline, trampoline_size, PROT_READ | PROT_EXEC) != 0) {
    perror("[logic2-bridge:inject] mprotect trampoline");
    munmap(trampoline, trampoline_size);
    return false;
  }
  original_on_data_buffer = (OnDataBufferFn)trampoline;

  if (!set_code_page_protection(target, hook_configuration.prologue_size,
                                PROT_READ | PROT_WRITE | PROT_EXEC)) {
    perror("[logic2-bridge:inject] mprotect GraphServer code");
    munmap(trampoline, trampoline_size);
    original_on_data_buffer = NULL;
    return false;
  }
  write_absolute_branch(target, (const void *)&inject_capture_data);
  memset(target + ABSOLUTE_BRANCH_SIZE, 0x90,
         hook_configuration.prologue_size - ABSOLUTE_BRANCH_SIZE);
  __builtin___clear_cache((char *)target,
                          (char *)target + hook_configuration.prologue_size);
  if (!set_code_page_protection(target, hook_configuration.prologue_size,
                                PROT_READ | PROT_EXEC)) {
    fprintf(stderr, "[logic2-bridge:inject] warning: could not restore code protection\n");
  }
  fprintf(stderr,
          "[logic2-bridge:inject] installed profile %s PXLogic hook at %p\n",
          hook_configuration.profile_id, (void *)target);
  return true;
#elif defined(_WIN32) && defined(_M_X64)
  HMODULE image_base = NULL;
  if (!GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                              GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                          (LPCSTR)exported_graph_symbol, &image_base) ||
      image_base == NULL) {
    fprintf(stderr,
            "[logic2-bridge:inject] could not locate GraphServer image base "
            "(error %lu)\n",
            (unsigned long)GetLastError());
    return false;
  }
  if (!verify_graph_codeview_identity(image_base)) return false;

  uint8_t *target =
      (uint8_t *)image_base + hook_configuration.on_data_buffer_offset;
  if (memcmp(target, hook_configuration.prologue,
             hook_configuration.prologue_size) != 0) {
    fprintf(stderr,
            "[logic2-bridge:inject] unsupported GraphServer: OnDataBuffer "
            "prologue mismatch at +0x%llx\n",
            (unsigned long long)hook_configuration.on_data_buffer_offset);
    return false;
  }

  const size_t trampoline_size =
      hook_configuration.prologue_size + ABSOLUTE_BRANCH_SIZE;
  uint8_t *trampoline =
      VirtualAlloc(NULL, trampoline_size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
  if (trampoline == NULL) {
    fprintf(stderr, "[logic2-bridge:inject] VirtualAlloc trampoline failed (error %lu)\n",
            (unsigned long)GetLastError());
    return false;
  }
  memcpy(trampoline, target, hook_configuration.prologue_size);
  write_absolute_branch(trampoline + hook_configuration.prologue_size,
                        target + hook_configuration.prologue_size);
  DWORD previous_trampoline_protection = 0;
  if (!set_code_page_protection(trampoline, trampoline_size, PAGE_EXECUTE_READ,
                                &previous_trampoline_protection)) {
    fprintf(stderr,
            "[logic2-bridge:inject] VirtualProtect trampoline failed (error %lu)\n",
            (unsigned long)GetLastError());
    VirtualFree(trampoline, 0, MEM_RELEASE);
    return false;
  }
  FlushInstructionCache(GetCurrentProcess(), trampoline, trampoline_size);
  original_on_data_buffer = (OnDataBufferFn)trampoline;

  DWORD previous_target_protection = 0;
  if (!set_code_page_protection(target, hook_configuration.prologue_size,
                                PAGE_EXECUTE_READWRITE,
                                &previous_target_protection)) {
    fprintf(stderr,
            "[logic2-bridge:inject] VirtualProtect GraphServer code failed "
            "(error %lu)\n",
            (unsigned long)GetLastError());
    VirtualFree(trampoline, 0, MEM_RELEASE);
    original_on_data_buffer = NULL;
    return false;
  }
  write_absolute_branch(target, (const void *)&inject_capture_data);
  memset(target + ABSOLUTE_BRANCH_SIZE, 0x90,
         hook_configuration.prologue_size - ABSOLUTE_BRANCH_SIZE);
  FlushInstructionCache(GetCurrentProcess(), target,
                        hook_configuration.prologue_size);
  DWORD discarded_protection = 0;
  if (!set_code_page_protection(target, hook_configuration.prologue_size,
                                previous_target_protection,
                                &discarded_protection)) {
    fprintf(stderr,
            "[logic2-bridge:inject] warning: could not restore code protection\n");
  }
  fprintf(stderr,
          "[logic2-bridge:inject] installed experimental profile %s PXLogic hook at %p\n",
          hook_configuration.profile_id, (void *)target);
  return true;
#else
  (void)exported_graph_symbol;
  fprintf(stderr,
          "[logic2-bridge:inject] GraphServer injection is unavailable on this platform\n");
  return false;
#endif
}

static uint32_t parse_port(const char *value) {
  char *end = NULL;
  errno = 0;
  unsigned long parsed = strtoul(value, &end, 10);
  if (errno != 0 || end == value || *end != '\0' || parsed > 65535) {
    fprintf(stderr, "[logic2-bridge:native] invalid port: %s\n", value);
    exit(2);
  }
  return (uint32_t)parsed;
}

#if defined(_WIN32)
static const char *python_home_environment_path(const char *path,
                                                char output[MAX_PATH]);
static bool set_windows_environment_utf8(const char *name, const char *value);
#endif

static void *load_graph_library(const char *path, const char *python_home_path) {
#if defined(_WIN32)
  // GraphServer imports Analyzer.dll, python314.dll, and several private
  // runtime DLLs from the selected Logic resource directory. Electron sets a
  // matching DLL search context before loading the graph addon; the portable
  // native host must establish it explicitly.
  wchar_t wide_path[MAX_PATH];
  int wide_length = MultiByteToWideChar(CP_UTF8, 0, path, -1,
                                        wide_path, MAX_PATH);
  if (wide_length <= 0) {
    // Node passes UTF-8 paths, but retain the system code page as a fallback
    // for direct command-line launches on older Windows configurations.
    wide_length = MultiByteToWideChar(CP_ACP, 0, path, -1,
                                      wide_path, MAX_PATH);
  }
  wchar_t directory[MAX_PATH];
  DWORD directory_length = wide_length > 0
      ? GetFullPathNameW(wide_path, MAX_PATH, directory, NULL) : 0;
  if (directory_length == 0 || directory_length >= MAX_PATH) {
    fprintf(stderr,
            "[logic2-bridge:native] could not resolve GraphServer path (error %lu)\n",
            (unsigned long)GetLastError());
  } else {
    wchar_t *separator = wcsrchr(directory, L'\\');
    if (separator == NULL) separator = wcsrchr(directory, L'/');
    if (separator != NULL) {
      *separator = '\0';
      if (!SetDllDirectoryW(directory)) {
        fprintf(stderr,
                "[logic2-bridge:native] SetDllDirectory failed for %s (error %lu)\n",
                path, (unsigned long)GetLastError());
      } else {
        char directory_utf8[MAX_PATH];
        int converted = WideCharToMultiByte(CP_UTF8, 0, directory, -1,
                                             directory_utf8, MAX_PATH, NULL, NULL);
        fprintf(stderr, "[logic2-bridge:native] DLL search directory: %s\n",
                converted > 0 ? directory_utf8 : path);
      }
    }
  }
  if (python_home_path != NULL) {
    char python_home_environment[MAX_PATH];
    const char *python_home = python_home_environment_path(
        python_home_path, python_home_environment);
    if (!set_windows_environment_utf8("SALEAE_PYTHONHOME", python_home)) {
      fprintf(stderr,
              "[logic2-bridge:native] SetEnvironmentVariableW(SALEAE_PYTHONHOME) failed\n");
      return NULL;
    }
  }
  HMODULE library = wide_length > 0
      ? LoadLibraryExW(wide_path, NULL, LOAD_WITH_ALTERED_SEARCH_PATH) : NULL;
  if (library == NULL) {
    fprintf(stderr, "[logic2-bridge:native] LoadLibraryEx failed (error %lu): %s\n",
            (unsigned long)GetLastError(), path);
  }
  return (void *)library;
#else
  (void)python_home_path;
  void *library = dlopen(path, RTLD_LAZY | RTLD_GLOBAL);
  if (library == NULL) {
    fprintf(stderr, "[logic2-bridge:native] dlopen failed: %s\n", dlerror());
  }
  return library;
#endif
}

static void unload_graph_library(void *library) {
#if defined(_WIN32)
  (void)FreeLibrary((HMODULE)library);
#else
  (void)dlclose(library);
#endif
}

static void bridge_sleep_one_second(void) {
#if defined(_WIN32)
  Sleep(1000);
#else
  sleep(1);
#endif
}

#if defined(_WIN32)
static const char *python_home_environment_path(const char *path,
                                                char output[MAX_PATH]) {
  DWORD length = GetFullPathNameA(path, MAX_PATH, output, NULL);
  if (length == 0 || length >= MAX_PATH) return path;
  for (char *cursor = output; *cursor != '\0'; cursor += 1) {
    if (*cursor == '\\') *cursor = '/';
  }
  // The official graph-interface addon passes a slash-normalized home to
  // SALEAE_PYTHONHOME without changing the path's trailing-separator state.
  return output;
}

static bool set_windows_environment_utf8(const char *name, const char *value) {
  wchar_t wide_name[MAX_PATH];
  wchar_t wide_value[MAX_PATH];
  if (MultiByteToWideChar(CP_UTF8, 0, name, -1, wide_name, MAX_PATH) <= 0 ||
      MultiByteToWideChar(CP_UTF8, 0, value, -1, wide_value, MAX_PATH) <= 0) {
    return false;
  }
  return SetEnvironmentVariableW(wide_name, wide_value) != 0;
}
#endif

int main(int argc, char **argv) {
  if (argc != 11) {
    fprintf(stderr,
            "Usage: %s <graph-library> <python-home> <log-path> "
            "<calibration-root> <port> <scan-devices> <profile-id> "
            "<graph-identity> <hook-offset> <hook-prologue-hex>\n",
            argv[0]);
    return 2;
  }
  setvbuf(stdout, NULL, _IONBF, 0);
#if defined(_WIN32)
  if (_setmode(_fileno(stdin), _O_BINARY) == -1) {
    perror("[logic2-bridge:native] set stdin binary mode");
    return 1;
  }
  WSADATA winsock_data;
  int winsock_status = WSAStartup(MAKEWORD(2, 2), &winsock_data);
  if (winsock_status != 0) {
    fprintf(stderr,
            "[logic2-bridge:native] WSAStartup failed (error %d)\n",
            winsock_status);
    return 1;
  }
  HRESULT com_status = CoInitializeEx(NULL, COINIT_MULTITHREADED);
  bool com_initialized = SUCCEEDED(com_status);
  if (FAILED(com_status) && com_status != RPC_E_CHANGED_MODE) {
    fprintf(stderr,
            "[logic2-bridge:native] CoInitializeEx failed (HRESULT 0x%08lx)\n",
            (unsigned long)com_status);
    WSACleanup();
    return 1;
  }
#endif
  if (!configure_hook(argv[7], argv[8], argv[9], argv[10])) return 2;
#if !defined(_WIN32)
  if (setenv("SALEAE_PYTHONHOME", argv[2], 1) != 0) {
    perror("[logic2-bridge:native] setenv SALEAE_PYTHONHOME");
    return 1;
  }
#endif

  void *library = load_graph_library(argv[1], argv[2]);
  if (library == NULL) {
    return 1;
  }
  CreateGraphServerFn create_graph_server =
      (CreateGraphServerFn)load_symbol(library, "CreateGraphServer");
  DestroyGraphServerFn destroy_graph_server =
      (DestroyGraphServerFn)load_symbol(library, "DestroyGraphServer");
  SetLogFileNameFn set_log_file_name =
      (SetLogFileNameFn)load_symbol(library, "SetLogFileName");
  FlushLogFn flush_log = (FlushLogFn)load_symbol(library, "FlushLog");

  if (!initialize_injection_ring()) {
    unload_graph_library(library);
    return 1;
  }
#if !defined(_WIN32)
  if (!start_injection_stdin_reader()) {
    unload_graph_library(library);
    return 1;
  }
#endif

  uint32_t port = parse_port(argv[5]);
  bool scan_devices = strcmp(argv[6], "1") == 0;
  set_log_file_name(argv[3]);
  fprintf(stderr, "[logic2-bridge:native] Python home: %s\n", argv[2]);
#if defined(_WIN32)
  DWORD python_attributes = GetFileAttributesA(argv[2]);
  fprintf(stderr,
          "[logic2-bridge:native] Python home %s: %s\n",
          argv[2],
          python_attributes == INVALID_FILE_ATTRIBUTES ?
              "missing" : ((python_attributes & FILE_ATTRIBUTE_DIRECTORY) ?
              "directory" : "file"));
  HMODULE analyzer = GetModuleHandleA("Analyzer.dll");
  HMODULE python = GetModuleHandleA("python314.dll");
  fprintf(stderr,
          "[logic2-bridge:native] dependencies before CreateGraphServer: "
          "Analyzer.dll=%s python314.dll=%s\n",
          analyzer != NULL ? "loaded" : "not loaded",
          python != NULL ? "loaded" : "not loaded");
#endif
  printf("[logic2-bridge:native] loading %s\n", argv[1]);
  printf("[logic2-bridge:native] physical Saleae scan: %s\n",
         scan_devices ? "enabled" : "disabled");

  void *server = create_graph_server(&port, 100, false, scan_devices, argv[4]);
  if (server == NULL) {
    fprintf(stderr, "[logic2-bridge:native] CreateGraphServer returned NULL\n");
    unload_graph_library(library);
    return 1;
  }
  // GraphServer construction can execute LogicDeviceNode initialization on
  // Windows. Patch the capture callback only after construction has finished;
  // physical scanning is disabled and PXLogic data is not armed yet, so no
  // capture callback can race this installation.
  if (!install_injection_hook((void *)create_graph_server)) {
    destroy_graph_server(server);
    unload_graph_library(library);
    return 1;
  }
  // A blocking read on Node's stdin pipe can interfere with the bundled
  // Python runtime while GraphServer is constructing. Do not touch the feeder
  // pipe until GraphServer has completed its initialization.
#if defined(_WIN32)
  if (!start_injection_stdin_reader()) {
    destroy_graph_server(server);
    unload_graph_library(library);
    return 1;
  }
#endif
  printf("GRAPH_WS_READY ws://127.0.0.1:%u/saleae\n", port);

  signal(SIGINT, handle_signal);
  signal(SIGTERM, handle_signal);
  while (!stop_requested) bridge_sleep_one_second();

  printf("[logic2-bridge:native] stopping\n");
  destroy_graph_server(server);
  (void)flush_log();
  unload_graph_library(library);
#if defined(_WIN32)
  if (com_initialized) CoUninitialize();
  WSACleanup();
#endif
  return 0;
}
