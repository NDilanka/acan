use crate::order::{Order, OrderState};

pub fn ship(order: &mut Order) {
    if order.state == OrderState::Paid {
        order.state = OrderState::Shipped;
    }
}
