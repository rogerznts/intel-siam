// Harness to drive Intelbras libt2u.so directly: register with the cloud, open a
// port tunnel to a device, report the local endpoint, and hold it alive.
//
//   cc -arch x86_64 -o t2u_tunnel t2u_tunnel.c
//   INTEL_SIAM_T2U_KEY=... INTEL_SIAM_T2U_PASSWORD=... ./t2u_tunnel <serial> <remote_port> <logdir>
//
// The cloud key and password SimPlay 1.4.1 hard-codes (TunnelingService) come from the environment.
// Prints "TUNNEL_READY <local_port>" on success, then blocks until SIGINT.
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CLOUD_ADDR "intelbrasp2p.com.br"
#define CLOUD_PORT 1250
#define LOOPBACK   "127.0.0.1"

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

int main(int argc, char **argv) {
	if (argc < 4) {
		fprintf(stderr, "usage: %s <serial> <remote_port> <logdir>\n", argv[0]);
		return 2;
	}
	const char    *serial      = argv[1];
	unsigned short remote_port = (unsigned short)atoi(argv[2]);
	const char    *logdir      = argv[3];

	void *h = dlopen("libt2u.so", RTLD_NOW);
	if (!h) { fprintf(stderr, "dlopen: %s\n", dlerror()); return 1; }

	t2u_init_v2_fn     t2u_init_v2     = (t2u_init_v2_fn)dlsym(h, "t2u_init_v2");
	t2u_add_port_v3_fn t2u_add_port_v3 = (t2u_add_port_v3_fn)dlsym(h, "t2u_add_port_v3");
	t2u_port_status_fn t2u_port_status = (t2u_port_status_fn)dlsym(h, "t2u_port_status");
	t2u_del_port_fn    t2u_del_port    = (t2u_del_port_fn)dlsym(h, "t2u_del_port");
	t2u_exit_fn        t2u_exit        = (t2u_exit_fn)dlsym(h, "t2u_exit");
	if (!t2u_init_v2 || !t2u_add_port_v3 || !t2u_port_status) {
		fprintf(stderr, "dlsym: missing export\n"); return 1;
	}

	const char *cloud_key = getenv("INTEL_SIAM_T2U_KEY");
	const char *cloud_pwd = getenv("INTEL_SIAM_T2U_PASSWORD");
	if (!cloud_key || !cloud_pwd) { fprintf(stderr, "INTEL_SIAM_T2U_KEY and INTEL_SIAM_T2U_PASSWORD must be set\n"); return 1; }

	t2u_init_v2(CLOUD_ADDR, CLOUD_PORT, cloud_key, logdir);

	int lp = t2u_add_port_v3(serial, cloud_pwd, LOOPBACK, remote_port, 0);
	printf("add_port_v3 -> %d\n", lp);
	if (lp <= 0 || lp > 65535) { fprintf(stderr, "add_port failed: %d\n", lp); return 1; }
	unsigned short local_port = (unsigned short)lp;

	T2uNetStat st; int status = 0;
	for (int i = 0; i < 100; i++) {
		usleep(100000);
		memset(&st, 0, sizeof st);
		status = t2u_port_status(local_port, &st);
		if (status != 0) break;
	}
	printf("port_status -> %d (ip=%s port=%d proxy=%d natR=%d natL=%d ver=%d)\n",
	       status, st.ip, st.port, st.proxy, st.remote_nattype, st.local_nattype, st.remote_version);

	if (status != 1) {
		fprintf(stderr, "tunnel not established (status %d)\n", status);
		if (t2u_del_port) t2u_del_port(local_port);
		if (t2u_exit) t2u_exit();
		return 1;
	}

	printf("TUNNEL_READY %u %s\n", local_port, st.proxy ? "RELAY" : "P2P");
	fflush(stdout);
	for (;;) pause();
	return 0;
}
