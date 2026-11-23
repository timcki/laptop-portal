// laptop-portal-agent opens drafts from tailnet peers in the local Zed
// and hands back the saved text once the tab is closed, and opens a peer's
// own files in Zed over ssh for reviews. it joins the tailnet
// as its own node through tsnet, so it is reachable only over tailscale, and
// serves only peers the tailnet policy grants its capability to.
package main

import (
	"context"
	"flag"
	"log"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"tailscale.com/tailcfg"
	"tailscale.com/tsnet"
)

func main() {
	// named before the repo became laptop-portal; it holds the tailnet login
	defaultDir := "edit-proposed-text-agent"
	if config, err := os.UserConfigDir(); err == nil {
		defaultDir = filepath.Join(config, defaultDir)
	}
	var (
		hostname = flag.String("hostname", "edit-agent", "tailnet machine name")
		dir      = flag.String("dir", defaultDir, "state directory for the tailnet node and drafts")
		addr     = flag.String("addr", ":8080", "tailnet address to listen on")
		capName  = flag.String("cap", "timcki.xyz/cap/edit-proposed-text", "peer capability callers must be granted")
		tag      = flag.String("tag", "tag:edit-agent", "tag to advertise for the node, empty for none")
		zed      = flag.String("zed", "", "zed cli path (default: autodetect)")
		verbose  = flag.Bool("verbose", false, "log tsnet internals")
	)
	flag.Parse()

	if *zed == "" {
		*zed = resolveZed()
	}
	edits := filepath.Join(*dir, "edits")
	if err := os.MkdirAll(edits, 0o700); err != nil {
		log.Fatalf("failed to create %s: %v", edits, err)
	}

	srv := &tsnet.Server{
		Hostname: *hostname,
		Dir:      filepath.Join(*dir, "tsnet"),
		// login urls and auth errors still reach UserLogf
		Logf: func(string, ...any) {},
	}
	if *verbose {
		srv.Logf = log.Printf
	}
	if *tag != "" {
		srv.AdvertiseTags = []string{*tag}
	}
	defer srv.Close()

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	if _, err := srv.Up(ctx); err != nil {
		log.Fatalf("failed to join the tailnet: %v", err)
	}
	lc, err := srv.LocalClient()
	if err != nil {
		log.Fatalf("failed to get the local client: %v", err)
	}
	ln, err := srv.Listen("tcp", *addr)
	if err != nil {
		log.Fatalf("failed to listen on %s: %v", *addr, err)
	}

	store := &editStore{
		dir:    edits,
		whois:  lc,
		cap:    tailcfg.PeerCapability(*capName),
		launch: zedLauncher(*zed, edits),
		open:   zedOpener(*zed),
	}
	store.failInterrupted()
	go func() {
		for range time.Tick(time.Hour) {
			store.prune()
		}
	}()

	httpSrv := &http.Server{Handler: store.routes(), ReadHeaderTimeout: 10 * time.Second}
	go func() {
		<-ctx.Done()
		httpSrv.Close()
	}()
	log.Printf("serving %s on %s%s with %s", *dir, *hostname, *addr, *zed)
	if err := httpSrv.Serve(ln); err != nil && err != http.ErrServerClosed {
		log.Fatal(err)
	}
}
