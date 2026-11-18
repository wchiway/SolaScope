package inventory

import "errors"

type Inventory struct {
	stock map[string]int
}

func NewInventory() *Inventory {
	return &Inventory{stock: map[string]int{}}
}

func (inventory *Inventory) Reserve(sku string, quantity int) error {
	// Reserve stock for an order and fail when the warehouse has too little.
	if inventory.stock[sku] < quantity {
		return errors.New("INSUFFICIENT_STOCK")
	}
	inventory.stock[sku] -= quantity
	return nil
}

func (inventory *Inventory) Restock(sku string, quantity int) {
	inventory.stock[sku] += quantity
}
