export enum TicketStatus {
  New,
  Open,
  Closed,
}

export function reopen(ticket: Ticket) {
  if (ticket.status === TicketStatus.Closed) {
    ticket.status = TicketStatus.Open;
  }
}

export function close(ticket: Ticket) {
  ticket.status = TicketStatus.Closed;
}
