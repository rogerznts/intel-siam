// Tunnel helper for the intel-siam viewer. libt2u.so is Mach-O x86_64, so this runs as an x86_64 child
// (Rosetta on Apple Silicon) and the arm64 app talks to it over pipes.
//
//   INTEL_SIAM_T2U_KEY=... INTEL_SIAM_T2U_PASSWORD=... t2u_tunnel <libt2u_path> <serial> <remote_port> <logdir>
//
// stdout is the protocol, one flushed line each:
//   READY <local_port> RELAY|P2P
//   DOWN <t2u_status>
// Diagnostics go to stderr. The helper lives exactly as long as its stdin: EOF (the app closed the pipe
// or died) removes the port and exits 0. A port status other than 1 prints DOWN and exits 1.
#include <dlfcn.h>
#include <errno.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CLOUD_ADDR "intelbrasp2p.com.br"
#define CLOUD_PORT 1250
#define LOOPBACK   "127.0.0.1"

#define T2U_CONNECTING   0
#define T2U_OK           1
#define T2U_PORT_UNAVAIL (-10)
#define T2U_TIMEOUT      (-11)

typedef struct {
	char  ip[20];
	int   port;
	int   proxy;
	float lost_rate;
	int   bandwidth;
	int   remote_nattype;
	int   local_nattype;
	char  ip6[40];
	int   remote_version;
} T2uNetStat;

typedef void (*t2u_init_v2_fn)(const char *addr, unsigned short port, const char *key, const char *logpath);
typedef int  (*t2u_add_port_v3_fn)(const char *uuid, const char *passwd, const char *remote_ip,
                                   unsigned short remote_port, unsigned short local_port);
typedef int  (*t2u_port_status_fn)(unsigned short port, T2uNetStat *stat);
typedef void (*t2u_del_port_fn)(unsigned short port);
typedef void (*t2u_exit_fn)(void);

static t2u_port_status_fn t2u_port_status;
static t2u_del_port_fn    t2u_del_port;
static t2u_exit_fn        t2u_exit;

static void down(int status) {
	printf("DOWN %d\n", status);
	fflush(stdout);
}

static void cleanup(unsigned short local_port) {
	if (local_port) t2u_del_port(local_port);
	t2u_exit();
}

// True once stdin reached EOF or errored; data on stdin is drained and ignored.
static int stdin_closed(int timeout_ms) {
	struct pollfd pfd = { .fd = STDIN_FILENO, .events = POLLIN };
	int r = poll(&pfd, 1, timeout_ms);
	if (r < 0) return errno != EINTR;
	if (r == 0) return 0;
	char buf[256];
	ssize_t n = read(STDIN_FILENO, buf, sizeof buf);
	return n <= 0 && !(n < 0 && errno == EINTR);
}

int main(int argc, char **argv) {
	if (argc != 5) {
		fprintf(stderr, "usage: %s <libt2u_path> <serial> <remote_port> <logdir>\n", argv[0]);
		return 2;
	}
	const char    *lib_path    = argv[1];
	const char    *serial      = argv[2];
	unsigned short remote_port = (unsigned short)atoi(argv[3]);
	const char    *logdir      = argv[4];
	const char    *cloud_key   = getenv("INTEL_SIAM_T2U_KEY");
	const char    *cloud_pwd   = getenv("INTEL_SIAM_T2U_PASSWORD");
	if (!cloud_key || !cloud_pwd) {
		fprintf(stderr, "INTEL_SIAM_T2U_KEY and INTEL_SIAM_T2U_PASSWORD must be set\n");
		return 2;
	}

	void *h = dlopen(lib_path, RTLD_NOW);
	if (!h) { fprintf(stderr, "dlopen: %s\n", dlerror()); return 2; }

	t2u_init_v2_fn     t2u_init_v2     = (t2u_init_v2_fn)dlsym(h, "t2u_init_v2");
	t2u_add_port_v3_fn t2u_add_port_v3 = (t2u_add_port_v3_fn)dlsym(h, "t2u_add_port_v3");
	t2u_port_status = (t2u_port_status_fn)dlsym(h, "t2u_port_status");
	t2u_del_port    = (t2u_del_port_fn)dlsym(h, "t2u_del_port");
	t2u_exit        = (t2u_exit_fn)dlsym(h, "t2u_exit");
	if (!t2u_init_v2 || !t2u_add_port_v3 || !t2u_port_status || !t2u_del_port || !t2u_exit) {
		fprintf(stderr, "dlsym: missing export\n");
		return 2;
	}

	t2u_init_v2(CLOUD_ADDR, CLOUD_PORT, cloud_key, logdir);

	int lp = t2u_add_port_v3(serial, cloud_pwd, LOOPBACK, remote_port, 0);
	fprintf(stderr, "add_port_v3 -> %d\n", lp);
	if (lp <= 0 || lp > 65535) {
		down(T2U_PORT_UNAVAIL);
		cleanup(0);
		return 1;
	}
	unsigned short local_port = (unsigned short)lp;

	// Same budget as SIMPlay's TunnelingService: 100 polls, 100 ms apart, while status is "connecting".
	T2uNetStat st;
	int status = T2U_CONNECTING;
	for (int i = 0; i < 100 && status == T2U_CONNECTING; i++) {
		if (stdin_closed(100)) { cleanup(local_port); return 0; }
		memset(&st, 0, sizeof st);
		status = t2u_port_status(local_port, &st);
	}
	if (status == T2U_CONNECTING) status = T2U_TIMEOUT;
	fprintf(stderr, "port_status -> %d (proxy=%d natR=%d natL=%d ver=%d)\n",
	        status, st.proxy, st.remote_nattype, st.local_nattype, st.remote_version);
	if (status != T2U_OK) {
		down(status);
		cleanup(local_port);
		return 1;
	}

	printf("READY %u %s\n", local_port, st.proxy ? "RELAY" : "P2P");
	fflush(stdout);

	for (;;) {
		if (stdin_closed(1000)) {
			cleanup(local_port);
			return 0;
		}
		status = t2u_port_status(local_port, &st);
		if (status != T2U_OK) {
			fprintf(stderr, "port_status -> %d\n", status);
			down(status);
			cleanup(local_port);
			return 1;
		}
	}
}
