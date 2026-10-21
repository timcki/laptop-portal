package main

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io/fs"
	"log"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"time"

	"tailscale.com/client/tailscale/apitype"
	"tailscale.com/tailcfg"
)

const (
	maxBodyBytes       = 1 << 20
	maxPendingPerOwner = 8
	maxPurposeChars    = 80
	maxSlugChars       = 60
	retention          = 7 * 24 * time.Hour
	// exit code recorded for edits whose editor was lost to an agent restart
	interruptedExitCode = -1
)

var (
	extensionPattern = regexp.MustCompile(`^[A-Za-z0-9]{1,16}$`)
	editIDPattern    = regexp.MustCompile(`^edit-[0-9a-f]{32}$`)
	slugSeparators   = regexp.MustCompile(`[^a-z0-9]+`)
)

// whoIser is the part of the tailscale local client the handlers need.
type whoIser interface {
	WhoIs(ctx context.Context, remoteAddr string) (*apitype.WhoIsResponse, error)
}

// launcher opens path in an editor and returns a wait func that blocks until
// the editor exits, with its exit code.
type launcher func(path string) (wait func() int, err error)

type editStore struct {
	dir    string
	whois  whoIser
	cap    tailcfg.PeerCapability
	launch launcher
	open   opener

	mu sync.Mutex // serializes creation so the pending limit holds
}

type editMeta struct {
	Owner     tailcfg.StableNodeID `json:"owner"`
	OwnerName string               `json:"owner_name"`
	File      string               `json:"file"`
	Original  string               `json:"original"`
	Created   time.Time            `json:"created"`
}

type createRequest struct {
	Text      string `json:"text"`
	Extension string `json:"extension"`
	Purpose   string `json:"purpose"`
}

type editResponse struct {
	EditID   string `json:"edit_id"`
	Status   string `json:"status"`
	Text     string `json:"text,omitempty"`
	Changed  *bool  `json:"changed,omitempty"`
	ExitCode *int   `json:"exit_code,omitempty"`
}

func (s *editStore) routes() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /v1/health", s.health)
	mux.HandleFunc("POST /v1/edits", s.create)
	mux.HandleFunc("GET /v1/edits/{id}", s.get)
	mux.HandleFunc("DELETE /v1/edits/{id}", s.cancel)
	mux.HandleFunc("POST /v1/open", s.openRemote)
	return mux
}

// caller resolves the tailnet peer behind r and requires the edit capability,
// so only nodes the tailnet policy grants it to can use the agent.
func (s *editStore) caller(w http.ResponseWriter, r *http.Request) (*apitype.WhoIsResponse, bool) {
	who, err := s.whois.WhoIs(r.Context(), r.RemoteAddr)
	if err != nil || who == nil || who.Node == nil {
		writeError(w, http.StatusForbidden, "unknown tailnet peer")
		return nil, false
	}
	if !who.CapMap.HasCapability(s.cap) {
		log.Printf("denied %s: missing %s", who.Node.Name, s.cap)
		writeError(w, http.StatusForbidden, "peer lacks "+string(s.cap))
		return nil, false
	}
	return who, true
}

func (s *editStore) health(w http.ResponseWriter, r *http.Request) {
	who, ok := s.caller(w, r)
	if !ok {
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"ok": true, "caller": who.Node.Name})
}

func (s *editStore) create(w http.ResponseWriter, r *http.Request) {
	who, ok := s.caller(w, r)
	if !ok {
		return
	}
	var req createRequest
	if err := json.NewDecoder(http.MaxBytesReader(w, r.Body, maxBodyBytes)).Decode(&req); err != nil {
		writeError(w, http.StatusBadRequest, "invalid request body")
		return
	}
	ext := req.Extension
	if ext == "" {
		ext = "md"
	}
	if !extensionPattern.MatchString(ext) {
		writeError(w, http.StatusBadRequest, "extension must be 1-16 ascii letters or digits")
		return
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	if s.pendingFor(who.Node.StableID) >= maxPendingPerOwner {
		writeError(w, http.StatusTooManyRequests, "too many open edits")
		return
	}

	id, err := newEditID()
	if err != nil {
		writeError(w, http.StatusInternalServerError, "failed to create edit id")
		return
	}
	dir := filepath.Join(s.dir, id)
	// the file name is what the editor tab shows, so it carries the purpose
	file := slug(normalizePurpose(req.Purpose)) + "." + ext
	meta := editMeta{
		Owner:     who.Node.StableID,
		OwnerName: who.Node.Name,
		File:      file,
		Original:  req.Text,
		Created:   time.Now(),
	}
	if err := writeEdit(dir, meta); err != nil {
		os.RemoveAll(dir)
		writeError(w, http.StatusInternalServerError, "failed to store draft")
		return
	}
	wait, err := s.launch(filepath.Join(dir, file))
	if err != nil {
		os.RemoveAll(dir)
		log.Printf("failed to launch editor: %v", err)
		writeError(w, http.StatusInternalServerError, "failed to launch editor")
		return
	}
	go func() {
		if err := writeAtomic(filepath.Join(dir, "editor.done"), strconv.Itoa(wait())+"\n"); err != nil {
			log.Printf("failed to record editor exit for %s: %v", id, err)
		}
	}()
	log.Printf("opened %s for %s as %s", file, who.Node.Name, id)
	writeJSON(w, http.StatusCreated, editResponse{EditID: id, Status: "pending"})
}

func (s *editStore) get(w http.ResponseWriter, r *http.Request) {
	who, ok := s.caller(w, r)
	if !ok {
		return
	}
	id, meta, ok := s.owned(w, r, who)
	if !ok {
		return
	}
	writeJSON(w, http.StatusOK, s.status(id, meta))
}

func (s *editStore) cancel(w http.ResponseWriter, r *http.Request) {
	who, ok := s.caller(w, r)
	if !ok {
		return
	}
	id, _, ok := s.owned(w, r, who)
	if !ok {
		return
	}
	if err := os.WriteFile(filepath.Join(s.dir, id, "canceled"), nil, 0o600); err != nil {
		writeError(w, http.StatusInternalServerError, "failed to cancel edit")
		return
	}
	writeJSON(w, http.StatusOK, editResponse{EditID: id, Status: "canceled"})
}

// owned loads the edit named in the path. edits of other nodes answer 404, as
// if they did not exist.
func (s *editStore) owned(w http.ResponseWriter, r *http.Request, who *apitype.WhoIsResponse) (string, editMeta, bool) {
	id := r.PathValue("id")
	if !editIDPattern.MatchString(id) {
		writeError(w, http.StatusNotFound, "edit not found")
		return "", editMeta{}, false
	}
	meta, err := readMeta(filepath.Join(s.dir, id))
	if err != nil || meta.Owner != who.Node.StableID {
		writeError(w, http.StatusNotFound, "edit not found")
		return "", editMeta{}, false
	}
	return id, meta, true
}

func (s *editStore) status(id string, meta editMeta) editResponse {
	dir := filepath.Join(s.dir, id)
	if exists(filepath.Join(dir, "canceled")) {
		return editResponse{EditID: id, Status: "canceled"}
	}
	raw, err := os.ReadFile(filepath.Join(dir, "editor.done"))
	if err != nil {
		return editResponse{EditID: id, Status: "pending"}
	}
	code, err := strconv.Atoi(strings.TrimSpace(string(raw)))
	if err != nil {
		code = interruptedExitCode
	}
	if code != 0 {
		return editResponse{EditID: id, Status: "failed", ExitCode: &code}
	}
	text, err := os.ReadFile(filepath.Join(dir, meta.File))
	if err != nil {
		code = interruptedExitCode
		return editResponse{EditID: id, Status: "failed", ExitCode: &code}
	}
	changed := string(text) != meta.Original
	return editResponse{EditID: id, Status: "completed", Text: string(text), Changed: &changed}
}

func (s *editStore) pendingFor(owner tailcfg.StableNodeID) int {
	n := 0
	s.each(func(id string, meta editMeta) {
		if meta.Owner == owner && s.status(id, meta).Status == "pending" {
			n++
		}
	})
	return n
}

// failInterrupted runs at startup: editors from a previous run can no longer report
// their exit, so their edits fail instead of staying pending forever. old
// finished edits are removed.
func (s *editStore) failInterrupted() {
	s.each(func(id string, meta editMeta) {
		dir := filepath.Join(s.dir, id)
		if s.status(id, meta).Status == "pending" {
			writeAtomic(filepath.Join(dir, "editor.done"), strconv.Itoa(interruptedExitCode)+"\n")
		}
	})
	s.prune()
}

func (s *editStore) prune() {
	s.each(func(id string, meta editMeta) {
		if time.Since(meta.Created) > retention && s.status(id, meta).Status != "pending" {
			os.RemoveAll(filepath.Join(s.dir, id))
		}
	})
}

func (s *editStore) each(fn func(id string, meta editMeta)) {
	entries, err := os.ReadDir(s.dir)
	if err != nil {
		return
	}
	for _, e := range entries {
		if !e.IsDir() || !editIDPattern.MatchString(e.Name()) {
			continue
		}
		if meta, err := readMeta(filepath.Join(s.dir, e.Name())); err == nil {
			fn(e.Name(), meta)
		}
	}
}

func writeEdit(dir string, meta editMeta) error {
	if err := os.Mkdir(dir, 0o700); err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(dir, meta.File), []byte(meta.Original), 0o600); err != nil {
		return err
	}
	raw, err := json.Marshal(meta)
	if err != nil {
		return err
	}
	return writeAtomic(filepath.Join(dir, "edit.json"), string(raw))
}

func readMeta(dir string) (editMeta, error) {
	var meta editMeta
	raw, err := os.ReadFile(filepath.Join(dir, "edit.json"))
	if err != nil {
		return meta, err
	}
	return meta, json.Unmarshal(raw, &meta)
}

func writeAtomic(path, text string) error {
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, []byte(text), 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

func exists(path string) bool {
	_, err := os.Stat(path)
	return !errors.Is(err, fs.ErrNotExist)
}

func newEditID() (string, error) {
	b := make([]byte, 16)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return "edit-" + hex.EncodeToString(b), nil
}

func normalizePurpose(purpose string) string {
	purpose = strings.Join(strings.Fields(purpose), " ")
	if purpose == "" {
		return "proposed text"
	}
	if r := []rune(purpose); len(r) > maxPurposeChars {
		purpose = string(r[:maxPurposeChars])
	}
	return purpose
}

func slug(purpose string) string {
	s := strings.Trim(slugSeparators.ReplaceAllString(strings.ToLower(purpose), "-"), "-")
	if len(s) > maxSlugChars {
		s = strings.TrimRight(s[:maxSlugChars], "-")
	}
	if s == "" {
		return "proposed-text"
	}
	return s
}

// zedLauncher runs the zed cli with --wait, which only returns once the tab
// is closed. zed reports a tab closed without saving as success, so the
// saved file is what counts.
func zedLauncher(zed string) launcher {
	return func(path string) (func() int, error) {
		cmd := exec.Command(zed, "--wait", "--new", path)
		if err := cmd.Start(); err != nil {
			return nil, err
		}
		return func() int {
			if err := cmd.Wait(); err != nil {
				var exitErr *exec.ExitError
				if errors.As(err, &exitErr) && exitErr.ExitCode() > 0 {
					return exitErr.ExitCode()
				}
				return interruptedExitCode
			}
			return 0
		}, nil
	}
}

func resolveZed() string {
	candidates := []string{
		"/Applications/Zed.app/Contents/MacOS/cli",
		"/Applications/Zed Preview.app/Contents/MacOS/cli",
		"/usr/local/bin/zed",
		"/opt/homebrew/bin/zed",
	}
	if home, err := os.UserHomeDir(); err == nil {
		candidates = append(candidates,
			filepath.Join(home, "Applications/Zed.app/Contents/MacOS/cli"),
			filepath.Join(home, ".local/bin/zed"),
		)
	}
	for _, c := range candidates {
		if info, err := os.Stat(c); err == nil && !info.IsDir() {
			return c
		}
	}
	if path, err := exec.LookPath("zed"); err == nil {
		return path
	}
	return "zed"
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(v)
}

func writeError(w http.ResponseWriter, status int, msg string) {
	writeJSON(w, status, map[string]string{"error": msg})
}
