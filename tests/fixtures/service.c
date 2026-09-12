#include <stddef.h>
int validate_name(const char *name) {
    return name != NULL && name[0] != '\0';
}
