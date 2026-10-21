package main

import (
	"encoding/json"
	"log"
	"net/http"
	"os/exec"
	"path"
	"regexp"
	"strings"
)

const maxRemotePath = 4096

var (
	sshUserPattern = regexp.MustCompile(`^[a-z_][a-z0-9_-]{0,31}$`)
	hostPattern    = regexp.MustCompile(`^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`)
	// plain path characters only, so the ssh url needs no escaping and zed
	// cannot read anything in it as url syntax
	remotePathPattern = regexp.MustCompile(`^/[A-Za-z0-9._~+@/-]*$`)
)

// opener hands a url to the local zed without waiting for it.
type opener func(url string) error

type openRequest struct {
	User string `json:"user"`
	Path string `json:"path"`
}

// openRemote opens a path on the calling server in zed over ssh, for reviews.
// the host is the caller's own tailnet name, never taken from the request, so
// a server can only open itself.
func (s *editStore) openRemote(w http.ResponseWriter, r *http.Request) {
	who, ok := s.caller(w, r)
	if !ok {
		return
	}
	var req openRequest
	if err := json.NewDecoder(http.MaxBytesReader(w, r.Body, 8<<10)).Decode(&req); err != nil {
		writeError(w, http.StatusBadRequest, "invalid request body")
		return
	}
	if !sshUserPattern.MatchString(req.User) {
		writeError(w, http.StatusBadRequest, "invalid ssh user")
		return
	}
	if len(req.Path) > maxRemotePath || !remotePathPattern.MatchString(req.Path) || path.Clean(req.Path) != req.Path {
		writeError(w, http.StatusBadRequest, "path must be absolute, clean and use plain characters")
		return
	}
	host := shortHost(who.Node.Name)
	if !hostPattern.MatchString(host) {
		writeError(w, http.StatusForbidden, "caller has no usable host name")
		return
	}
	url := "ssh://" + req.User + "@" + host + req.Path
	if err := s.open(url); err != nil {
		log.Printf("failed to open %s: %v", url, err)
		writeError(w, http.StatusInternalServerError, "failed to launch zed")
		return
	}
	log.Printf("opened %s for %s", url, who.Node.Name)
	writeJSON(w, http.StatusOK, map[string]string{"opened": url})
}

// shortHost is the first label of a magicdns name, the name the laptop's ssh
// config and known_hosts already use for the server.
func shortHost(name string) string {
	host, _, _ := strings.Cut(strings.TrimSuffix(name, "."), ".")
	return strings.ToLower(host)
}

// zedOpener runs the zed cli on an ssh url. zed answers urls without
// waiting, so the process is only reaped.
func zedOpener(zed string) opener {
	return func(url string) error {
		cmd := exec.Command(zed, url)
		if err := cmd.Start(); err != nil {
			return err
		}
		go func() {
			if err := cmd.Wait(); err != nil {
				log.Printf("zed exited for %s: %v", url, err)
			}
		}()
		return nil
	}
}
