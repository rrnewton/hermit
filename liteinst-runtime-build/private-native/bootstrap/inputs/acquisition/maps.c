#include "acquire.h"

static int number(const unsigned char **cursor, const unsigned char *end,
                  unsigned int base, uintptr_t *value) {
  const unsigned char *start = *cursor;
  uintptr_t result = 0;
  while (*cursor < end) {
    unsigned int digit = **cursor;
    if (digit >= '0' && digit <= '9') digit -= '0';
    else if (base == 16 && digit >= 'a' && digit <= 'f') digit -= 'a' - 10;
    else if (base == 16 && digit >= 'A' && digit <= 'F') digit -= 'A' - 10;
    else break;
    if (digit >= base || result > (UINTPTR_MAX - digit) / base) return 0;
    result = result * base + digit;
    ++*cursor;
  }
  *value = result;
  return *cursor != start;
}

static int take(const unsigned char **cursor, const unsigned char *end,
                unsigned char expected) {
  if (*cursor == end || **cursor != expected) return 0;
  ++*cursor;
  return 1;
}

static int spaces(const unsigned char **cursor, const unsigned char *end) {
  if (!take(cursor, end, ' ')) return 0;
  while (*cursor < end && **cursor == ' ') ++*cursor;
  return 1;
}

enum ma_status ma_stack_extent(const unsigned char *maps, size_t size,
                               uintptr_t rsp, struct ps_view *view) {
  if (maps == NULL || view == NULL || size == 0 || size > MA_MAPS_LIMIT || rsp == 0)
    return MA_ARGUMENT;
  const unsigned char *cursor = maps, *end = maps + size;
  uintptr_t last_end = 0, found_end = 0;
  unsigned int lines = 0;
  while (cursor < end) {
    uintptr_t begin, finish, ignored;
    if (++lines > 16384 || !number(&cursor, end, 16, &begin) ||
        !take(&cursor, end, '-') || !number(&cursor, end, 16, &finish) ||
        begin >= finish || begin < last_end || (begin & 4095) != 0 ||
        (finish & 4095) != 0 || !spaces(&cursor, end) || end - cursor < 4)
      return MA_MAPS;
    int readable = cursor[0] == 'r';
    if ((!readable && cursor[0] != '-') ||
        (cursor[1] != 'w' && cursor[1] != '-') ||
        (cursor[2] != 'x' && cursor[2] != '-') ||
        (cursor[3] != 'p' && cursor[3] != 's')) return MA_MAPS;
    cursor += 4;
    if (!spaces(&cursor, end) || !number(&cursor, end, 16, &ignored) ||
        !spaces(&cursor, end) || !number(&cursor, end, 16, &ignored) ||
        !take(&cursor, end, ':') || !number(&cursor, end, 16, &ignored) ||
        !spaces(&cursor, end) || !number(&cursor, end, 10, &ignored)) return MA_MAPS;
    if (cursor == end || (*cursor != ' ' && *cursor != '\n')) return MA_MAPS;
    while (cursor < end && *cursor != '\n') {
      if (*cursor == 0) return MA_MAPS;
      ++cursor;
    }
    if (!take(&cursor, end, '\n')) return MA_MAPS;
    if (begin <= rsp && rsp < finish) {
      if (!readable || found_end != 0) return MA_STACK;
      found_end = finish;
    }
    last_end = finish;
  }
  if (found_end == 0 || found_end - rsp > PS_MAX_STACK) return MA_STACK;
  *view = (struct ps_view) {(const unsigned char *)rsp, found_end - rsp};
  return MA_OK;
}
