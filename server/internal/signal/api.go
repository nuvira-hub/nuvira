package signal

import (
	"errors"

	"github.com/alexbn71/NexoDesk/server/internal/accounts"
	"github.com/alexbn71/NexoDesk/server/internal/pb"
)

// api risponde a una richiesta su account e rubrica. Prima di tutto serve un
// accesso (Login o LoginToken), che resta legato alla connessione.
func (s *Server) api(c *client, req *pb.ApiRequest) {
	resp := &pb.ApiResponse{Id: req.Id}
	if err := s.apiCall(c, req, resp); err != nil {
		if !errors.Is(err, accounts.ErrDenied) {
			s.log.Warn("richiesta alla rubrica non riuscita", "dispositivo", c.device.ID, "err", err)
		}
		resp.Result = nil
		resp.Error = err.Error()
	}
	_ = c.send(&pb.ServerMessage{Msg: &pb.ServerMessage_Api{Api: resp}})
}

func (s *Server) apiCall(c *client, req *pb.ApiRequest, resp *pb.ApiResponse) error {
	if s.accounts == nil {
		return errors.New("@server.no_rubrica")
	}
	switch call := req.Call.(type) {
	case *pb.ApiRequest_Login:
		session, err := s.accounts.Login(call.Login.Username, call.Login.Password)
		if err != nil {
			s.log.Info("accesso alla rubrica rifiutato", "utente", call.Login.Username, "da", c.conn.RemoteAddr())
			return err
		}
		s.log.Info("accesso alla rubrica", "utente", session.Account.Username, "dispositivo", c.device.ID)
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
	default:
		return errors.New("richiesta sconosciuta")
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
