#include "../include/holt_ffi.h"

#include <stdint.h>

int main(void) {
  HoltTree *tree = 0;
  const uint8_t key[] = {'s', 'm', 'o', 'k', 'e'};
  const uint8_t value[] = {'o', 'k'};

  if (holt_tree_open_memory(&tree) != HOLT_OK) {
    return 1;
  }
  if (holt_tree_put(tree, key, sizeof(key), value, sizeof(value)) != HOLT_OK) {
    holt_tree_close(tree);
    return 1;
  }

  HoltRecord record = {0};
  if (holt_tree_get(tree, key, sizeof(key), &record) != HOLT_OK || !record.found) {
    holt_tree_close(tree);
    return 1;
  }

  holt_record_free(&record);
  holt_tree_close(tree);
  return 0;
}
