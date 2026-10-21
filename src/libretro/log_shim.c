// Stable Rust cannot define C-variadic functions, so the core's printf-style
// log callback lands here and is formatted before it reaches Rust.
#include <stdarg.h>
#include <stddef.h>
#include <stdio.h>

void termcart_log_line(int level, const char *msg, size_t len);

void termcart_log_printf(int level, const char *fmt, ...) {
    char buf[1024];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    if (n < 0) {
        return;
    }
    if ((size_t)n >= sizeof buf) {
        n = sizeof buf - 1;
    }
    termcart_log_line(level, buf, (size_t)n);
}
