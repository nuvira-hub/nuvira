package signal

import (
	"errors"

	"github.com/nuvira-hub/nuvira/server/internal/accounts"
	"github.com/nuvira-hub/nuvira/server/internal/pb"
)

// api answers a request about accounts and address book. First of all a sign-in
// is needed (Login or LoginToken), which stays bound to the connection.
func (s *Server) api(c *client, req *pb.ApiRequest) {
	resp := &pb.ApiResponse{Id: req.Id}
	if err := s.apiCall(c, req, resp); err != nil {
		if !errors.Is(err, accounts.ErrDenied) {
			s.log.Warn("address book request failed", "device", c.device.ID, "err", err)
		}
		resp.Result = nil
		resp.Error = err.Error()
	}
	_ = c.send(&pb.ServerMessage{Msg: &pb.ServerMessage_Api{Api: resp}})
}

func (s *Server) apiCall(c *client, req *pb.ApiRequest, resp *pb.ApiResponse) error {
	if s.accounts == nil {
		return errors.New("@server.no_address_book")
	}
	switch call := req.Call.(type) {
	case *pb.ApiRequest_Login:
		session, err := s.accounts.Login(call.Login.Username, call.Login.Password)
		if err != nil {
			s.log.Info("address book sign-in rejected", "user", call.Login.Username, "from", c.conn.RemoteAddr())
			return err
		}
		s.log.Info("address book sign-in", "user", session.Account.Username, "device", c.device.ID)
		c.setSession(session)
		resp.Result = &pb.ApiResponse_Session{Session: session}
		return nil
	case *pb.ApiRequest_LoginToken:
		session, err := s.accounts.LoginToken(call.LoginToken.Token)
		if err != nil {
			return err
		}
		c.setSession(session)
		resp.Result = &pb.ApiResponse_Session{Session: session}
		return nil
	}

	account, token := c.currentSession()
	if account == nil {
		return errors.New("@err.login_required")
	}
	switch call := req.Call.(type) {
	case *pb.ApiRequest_Logout:
		c.setSession(nil)
		return s.accounts.Logout(token)
	case *pb.ApiRequest_ChangePassword:
		return s.accounts.ChangePassword(account, call.ChangePassword.OldPassword, call.ChangePassword.NewPassword)
	case *pb.ApiRequest_GetBooks:
		books, err := s.accounts.Books(account)
		if err == nil {
			s.presence(books)
		}
		resp.Result = &pb.ApiResponse_Books{Books: books}
		return err
	case *pb.ApiRequest_PutEntry:
		entry, err := s.accounts.PutEntry(account, call.PutEntry)
		resp.Result = &pb.ApiResponse_Entry{Entry: entry}
		return err
	case *pb.ApiRequest_AutoAdd:
		entry, err := s.accounts.AutoAdd(account, call.AutoAdd)
		if entry != nil {
			resp.Result = &pb.ApiResponse_Entry{Entry: entry}
		}
		return err
	case *pb.ApiRequest_DeleteEntry:
		return s.accounts.DeleteEntry(account, call.DeleteEntry.Id)
	case *pb.ApiRequest_ListAccounts:
		list, err := s.accounts.ListAccounts(account)
		resp.Result = &pb.ApiResponse_Accounts{Accounts: list}
		return err
	case *pb.ApiRequest_PutAccount:
		saved, err := s.accounts.PutAccount(account, call.PutAccount)
		resp.Result = &pb.ApiResponse_Account{Account: saved}
		return err
	case *pb.ApiRequest_DeleteAccount:
		return s.accounts.DeleteAccount(account, call.DeleteAccount.Id)
	case *pb.ApiRequest_PutBook:
		book, err := s.accounts.PutBook(account, call.PutBook)
		resp.Result = &pb.ApiResponse_Book{Book: book}
		return err
	case *pb.ApiRequest_DeleteBook:
		return s.accounts.DeleteBook(account, call.DeleteBook.Id)
	case *pb.ApiRequest_GetPolicy:
		policy, err := s.accounts.Policy()
		resp.Result = &pb.ApiResponse_Policy{Policy: policy}
		return err
	case *pb.ApiRequest_PutPolicy:
		policy, err := s.accounts.PutPolicy(account, call.PutPolicy)
		if err == nil {
			s.broadcastPolicy(policy)
		}
		resp.Result = &pb.ApiResponse_Policy{Policy: policy}
		return err
	default:
		return errors.New("unknown request")
	}
}

func (c *client) setSession(session *pb.Session) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.session = session
}

func (c *client) currentSession() (*pb.Account, string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.session == nil {
		return nil, ""
	}
	return c.session.Account, c.session.Token
}

// presence fills in whether each device of the books is connected now, or when it
// was last seen.
func (s *Server) presence(books *pb.Books) {
	s.mu.Lock()
	online := make(map[string]bool, len(s.online))
	for id := range s.online {
		online[id] = true
	}
	s.mu.Unlock()
	for _, book := range books.GetBooks() {
		for _, e := range book.GetEntries() {
			e.Online = online[e.GetDeviceId()]
			if d, err := s.store.ByID(e.GetDeviceId()); err == nil && d != nil {
				e.LastSeen = d.LastSeen.Unix()
			}
		}
	}
}

// broadcastPolicy sends the new rules to every device connected.
func (s *Server) broadcastPolicy(policy *pb.Policy) {
	s.mu.Lock()
	devices := make([]*client, 0, len(s.online))
	for _, c := range s.online {
		devices = append(devices, c)
	}
	s.mu.Unlock()
	for _, c := range devices {
		_ = c.send(&pb.ServerMessage{Msg: &pb.ServerMessage_Policy{Policy: policy}})
	}
}
