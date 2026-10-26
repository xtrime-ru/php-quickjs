#include <php.h>
#include <Zend/zend_exceptions.h>
#include <main/php_streams.h>
#include <main/streams/php_stream_plain_wrapper.h>
#include <fcntl.h>
#include <unistd.h>

bool php_quickjs_is_unwinding(void)
{
    return EG(exception) && (zend_is_unwind_exit(EG(exception)) || zend_is_graceful_exit(EG(exception)));
}
#ifdef __linux__
#include <sys/eventfd.h>
int php_quickjs_notification_fd(void) {
    return eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
}
#endif

/* PHP owns a duplicate, so closing its stream cannot close the Rust socket.
 * Using the streams API also supports FPM/CGI, unlike php://fd. */
bool php_quickjs_notification_stream(int fd, zval *result)
{
    int owned = fcntl(fd, F_DUPFD_CLOEXEC, 0);
    if (owned < 0) return false;
    php_stream *stream = php_stream_fopen_from_fd(owned, "r", NULL);
    if (stream == NULL) {
        close(owned);
        return false;
    }
    php_stream_to_zval(stream, result);
    return true;
}
