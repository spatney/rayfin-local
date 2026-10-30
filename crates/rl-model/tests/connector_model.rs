//! A Category A connector's analysis (`js/analyze.mjs`, connectors mode) as an `AppModel`: the
//! `inventory` connector of the `connectors` fixture app.

use rl_model::{Action, AppModel, Cardinality, CompareOp, FieldKind, Operand, PolicyExpr};
use serde_json::Value;

fn inventory() -> AppModel {
    let json = include_str!("fixtures/inventory.connector.json");
    AppModel::from_connector_analysis(&serde_json::from_str(json).unwrap()).unwrap()
}

fn metadata() -> Value {
    let json = include_str!(
        "../../../tests/fixtures/apps/connectors/rayfin/connectors/inventory/metadata.json"
    );
    serde_json::from_str(json).unwrap()
}

#[test]
fn entities_map_to_their_source_objects_and_keys() {
    let model = inventory();
    let product = model.entity("Product").unwrap();
    assert_eq!(product.table, "sales.products");
    assert_eq!(product.graphql.list_query, "products");
    assert_eq!(product.graphql.by_pk_query, "product_by_pk");
    let key: Vec<_> = product.key_columns().iter().map(|c| &c.field).collect();
    assert_eq!(key, ["productId"]);
    let price = product.field("price").unwrap();
    assert!(matches!(
        price.kind,
        FieldKind::Decimal {
            precision: 10,
            scale: 2,
            ..
        }
    ));
    assert_eq!(product.field("inStock").unwrap().name, "in_stock");

    let stock = model.entity("StockLevel").unwrap();
    let key: Vec<_> = stock.key_columns().iter().map(|c| &c.name).collect();
    assert_eq!(key, ["product_id", "warehouse_code"]);

    let audit = model.entity("AuditEntry").unwrap();
    assert!(audit.key_columns().is_empty());
    assert!(audit.columns.iter().all(|c| !c.primary_key));
}

#[test]
fn relationships_and_permissions_come_from_the_decorators() {
    let model = inventory();
    let product = model.entity("Product").unwrap();
    let category = product.relationship("category").unwrap();
    assert_eq!(category.cardinality, Cardinality::One);
    assert_eq!(category.source_columns, ["category_id"]);
    assert_eq!(category.target_columns, ["category_id"]);
    let stock = product.relationship("stockLevels").unwrap();
    assert_eq!(stock.cardinality, Cardinality::Many);

    let update = product
        .grants("authenticated", Action::Update)
        .next()
        .unwrap();
    let policy = update.policy.as_ref().unwrap();
    assert_eq!(policy.text, "@claims.email eq @item.ownerEmail");
    assert_eq!(
        policy.expr,
        PolicyExpr::Compare {
            op: CompareOp::Eq,
            left: Operand::Claim("email".into()),
            right: Operand::Field("ownerEmail".into()),
        }
    );
    let anonymous = product.grants("anonymous", Action::Read).next().unwrap();
    assert!(!anonymous.fields.as_ref().unwrap().allows("ownerEmail"));
}

#[test]
fn source_metadata_marks_database_types_and_generated_columns() {
    let mut model = inventory();
    assert!(model.apply_source_metadata(&metadata()).is_empty());
    let product = model.entity("Product").unwrap();
    let id = product.field("productId").unwrap().source.as_ref().unwrap();
    assert_eq!(id.data_type, "uniqueidentifier");
    assert!(id.has_default && !id.generated);
    let category = model.entity("Category").unwrap();
    let key = category
        .field("categoryId")
        .unwrap()
        .source
        .as_ref()
        .unwrap();
    assert!(key.generated);
    let audit = model.entity("AuditEntry").unwrap();
    let at = audit.field("at").unwrap().source.as_ref().unwrap();
    assert_eq!(at.data_type, "datetimeoffset");
}
