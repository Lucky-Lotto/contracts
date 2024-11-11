use anchor_lang::prelude::*;
use anchor_lang::solana_program::hash::hash;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};
use solana_program::clock::Clock;

declare_id!("4Vi9n94NDfjgd4d4ETVKKfsPYym1ugenokxNt6TtyGth");

pub const SECONDS_PER_MINUTE: i64 = 60;
pub const MINUTES_PER_HOUR: i64 = 60;
pub const DRAW_WINDOW_MINUTES: i64 = 5; // Lottery window (minutes)
pub const MIN_DRAW_INTERVAL: i64 = 55 * SECONDS_PER_MINUTE; // Minimum draw interval
pub const LOCK_DURATION: i64 = 5 * SECONDS_PER_MINUTE; // 5 minute lock period
pub const NUMBERS_COUNT: usize = 3; // 3D lottery requires 3 numbers
pub const MAX_NUMBER: u8 = 33; // maximum number
pub const MIN_NUMBER: u8 = 1; // minimum number

#[program]
pub mod lottery_3d_contract {
    use super::*;

    pub fn initialize(
        ctx: Context<Initialize>,
        min_purchase_amount: u32,
        token_mint: Pubkey,
    ) -> Result<()> {
        let lottery = &mut ctx.accounts.lottery;
        lottery.authority = ctx.accounts.authority.key();
        lottery.token_account = ctx.accounts.token_account.key();
        lottery.token_mint = token_mint;
        lottery.last_draw_time = 0;
        lottery.is_locked = false;
        lottery.min_purchase_amount = min_purchase_amount;
        lottery.last_draw_numbers = [0; NUMBERS_COUNT];
        lottery.last_prize_amount = 0;

        emit!(LotteryInitialized {
            authority: lottery.authority,
            token_mint,
            min_purchase_amount,
        });

        Ok(())
    }

    pub fn buy_ticket(
        ctx: Context<BuyTicket>,
        numbers: [u8; NUMBERS_COUNT],
        amount: u64,
    ) -> Result<()> {
        let clock = Clock::get()?;
        let current_timestamp = clock.unix_timestamp;
        let lottery = &mut ctx.accounts.lottery;

        require!(
            !lottery.is_in_draw_window(current_timestamp),
            LotteryError::DrawWindowActive
        );

        require!(
            ctx.accounts.lottery_token_account.mint == lottery.token_mint
                && ctx.accounts.buyer_token_account.mint == lottery.token_mint,
            LotteryError::InvalidTokenMint
        );

        if lottery.is_locked {
            let time_since_last_draw = current_timestamp - lottery.last_draw_time;
            require!(time_since_last_draw > LOCK_DURATION, LotteryError::Locked);
            lottery.is_locked = false;
        }

        require!(
            amount >= lottery.min_purchase_amount as u64,
            LotteryError::InsufficientAmount
        );

        require!(
            validate_ticket_numbers(&numbers),
            LotteryError::InvalidTicketNumbers
        );

        let transfer_ctx = CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.buyer_token_account.to_account_info(),
                to: ctx.accounts.lottery_token_account.to_account_info(),
                authority: ctx.accounts.buyer.to_account_info(),
            },
        );
        token::transfer(transfer_ctx, amount)?;

        emit!(TicketPurchased {
            buyer: ctx.accounts.buyer.key(),
            numbers,
            amount,
            timestamp: current_timestamp,
        });

        Ok(())
    }

    pub fn draw(ctx: Context<Draw>, uuid: String) -> Result<()> {
        let clock = Clock::get()?;
        let current_timestamp = clock.unix_timestamp;
        let lottery = &mut ctx.accounts.lottery;

        require!(
            lottery.is_in_draw_window(current_timestamp),
            LotteryError::InvalidDrawTime
        );

        require!(
            lottery.can_draw(current_timestamp),
            LotteryError::DrawTooEarly
        );

        require!(!lottery.is_locked, LotteryError::AlreadyDrawn);

        let recent_blockhashes = ctx.accounts.recent_blockhashes.try_borrow_data()?;
        let random_value = generate_vrf_random_number(
            current_timestamp,
            &recent_blockhashes,
            &lottery.to_account_info(),
            &uuid,
        )?;

        let draw_numbers = convert_random_to_3d_numbers(&random_value);

        lottery.last_draw_time = current_timestamp;
        lottery.is_locked = true;
        lottery.last_draw_numbers = draw_numbers;

        emit!(DrawResult {
            numbers: draw_numbers,
            draw_time: lottery.last_draw_time,
        });

        Ok(())
    }

    pub fn update_prize_amount(ctx: Context<UpdatePrize>, prize_amount: u64) -> Result<()> {
        let lottery = &mut ctx.accounts.lottery;
        let clock = Clock::get()?;
        let current_timestamp = clock.unix_timestamp;

        require!(
            lottery.is_locked && current_timestamp - lottery.last_draw_time <= LOCK_DURATION,
            LotteryError::PrizeUpdateWindowClosed
        );

        require!(prize_amount > 0, LotteryError::InvalidPrizeAmount);

        lottery.last_prize_amount += prize_amount;

        emit!(PrizeAmountUpdated {
            amount: prize_amount,
            draw_time: lottery.last_draw_time,
        });

        Ok(())
    }

    pub fn withdraw_sol(ctx: Context<WithdrawSol>, amount: u64) -> Result<()> {
        let rent_balance =
            Rent::get()?.minimum_balance(ctx.accounts.lottery.to_account_info().data_len());

        let available_balance = ctx
            .accounts
            .lottery
            .to_account_info()
            .lamports()
            .checked_sub(rent_balance)
            .ok_or(LotteryError::InsufficientBalance)?;

        require!(
            amount <= available_balance,
            LotteryError::InsufficientBalance
        );

        **ctx
            .accounts
            .lottery
            .to_account_info()
            .try_borrow_mut_lamports()? -= amount;
        **ctx.accounts.authority.try_borrow_mut_lamports()? += amount;

        emit!(SolWithdrawn {
            amount,
            authority: ctx.accounts.authority.key(),
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    pub fn transfer_token<'info>(
        ctx: Context<'_, '_, '_, 'info, TransferToken<'info>>,
        transfers: Vec<TransferInfo>,
        total_amount: u64,
    ) -> Result<()> {
        require!(total_amount > 0, LotteryError::InsufficientPrizeAmount);
        require!(
            ctx.accounts.lottery.last_prize_amount >= total_amount,
            LotteryError::InsufficientPrizeAmount
        );

        let auth_key = ctx.accounts.authority.key();

        for (i, transfer) in transfers.iter().enumerate() {
            let recipient_account = ctx
                .remaining_accounts
                .get(i)
                .ok_or(LotteryError::InvalidTokenMint)?;

            let signer_seeds: &[&[&[u8]]] =
                &[&[b"lottery", auth_key.as_ref(), &[ctx.bumps.lottery]]];

            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    token::Transfer {
                        from: ctx.accounts.lottery_token_account.to_account_info(),
                        to: recipient_account.to_account_info(),
                        authority: ctx.accounts.lottery.to_account_info(),
                    },
                    signer_seeds,
                ),
                transfer.amount,
            )?;

            emit!(TokenDrawTransfer {
                amount: transfer.amount,
                recipient: transfer.recipient,
                remaining_prize: ctx.accounts.lottery.last_prize_amount - transfer.amount,
            });
        }

        let lottery = &mut ctx.accounts.lottery;
        lottery.last_prize_amount = lottery
            .last_prize_amount
            .checked_sub(total_amount)
            .ok_or(LotteryError::ArithmeticError)?;

        emit!(BatchTransferCompleted {
            total_amount,
            transfer_count: transfers.len() as u8,
            remaining_prize: lottery.last_prize_amount,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }
}

#[account]
#[derive(Default)]
pub struct LotteryState {
    pub authority: Pubkey,
    pub token_account: Pubkey,
    pub token_mint: Pubkey,
    pub last_draw_time: i64,
    pub is_locked: bool,
    pub min_purchase_amount: u32,
    pub last_draw_numbers: [u8; NUMBERS_COUNT],
    pub last_prize_amount: u64,
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + 32 + 32 + 32 + 8 + 1 + 4 + 3 + 8 + 8 + 8 + 8,
        seeds = [b"lottery", authority.key().as_ref()],
        bump
    )]
    pub lottery: Account<'info, LotteryState>,

    #[account(
        init,
        payer = authority,
        seeds = [b"token_account", authority.key().as_ref()],
        bump,
        token::mint = token_mint,
        token::authority = lottery
    )]
    pub token_account: Box<Account<'info, TokenAccount>>,

    /// CHECK: Token mint account
    pub token_mint: AccountInfo<'info>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct BuyTicket<'info> {
    #[account(
        mut,
        seeds = [b"lottery", lottery.authority.as_ref()],
        bump
    )]
    pub lottery: Account<'info, LotteryState>,

    #[account(
        mut,
        seeds = [b"token_account", lottery.authority.as_ref()],
        bump,
        token::mint = lottery.token_mint,
        token::authority = lottery
    )]
    pub lottery_token_account: Account<'info, TokenAccount>,

    #[account(mut)]
    pub buyer_token_account: Account<'info, TokenAccount>,
    pub buyer: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Draw<'info> {
    #[account(
        mut,
        seeds = [b"lottery", lottery.authority.as_ref()],
        bump,
        has_one = authority
    )]
    pub lottery: Account<'info, LotteryState>,

    /// CHECK: Recent blockhashes account for VRF
    #[account(address = solana_program::sysvar::recent_blockhashes::ID)]
    pub recent_blockhashes: AccountInfo<'info>,

    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct UpdatePrize<'info> {
    #[account(
        mut,
        seeds = [b"lottery", lottery.authority.as_ref()],
        bump,
        has_one = authority
    )]
    pub lottery: Account<'info, LotteryState>,
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct WithdrawSol<'info> {
    #[account(
        mut,
        seeds = [b"lottery", lottery.authority.as_ref()],
        bump,
        has_one = authority
    )]
    pub lottery: Account<'info, LotteryState>,

    #[account(mut)]
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
pub struct TransferToken<'info> {
    #[account(
        mut,
        seeds = [b"lottery", authority.key().as_ref()],
        bump,
        has_one = authority,
    )]
    pub lottery: Account<'info, LotteryState>,

    #[account(
        mut,
        seeds = [b"token_account", authority.key().as_ref()],
        bump,
        token::mint = mint.key(),
        token::authority = lottery,
    )]
    pub lottery_token_account: Account<'info, TokenAccount>,

    /// CHECK: Token mint account, verified in the token_account constraint
    pub mint: AccountInfo<'info>,

    pub authority: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct TransferInfo {
    pub recipient: Pubkey,
    pub amount: u64,
}

#[event]
pub struct LotteryInitialized {
    pub authority: Pubkey,
    pub token_mint: Pubkey,
    pub min_purchase_amount: u32,
}

#[event]
pub struct TicketPurchased {
    pub buyer: Pubkey,
    pub numbers: [u8; NUMBERS_COUNT],
    pub amount: u64,
    pub timestamp: i64,
}

#[event]
pub struct DrawResult {
    pub numbers: [u8; NUMBERS_COUNT],
    pub draw_time: i64,
}

#[event]
pub struct PrizeAmountUpdated {
    pub amount: u64,
    pub draw_time: i64,
}

#[event]
pub struct TokenDrawTransfer {
    pub amount: u64,
    pub recipient: Pubkey,
    pub remaining_prize: u64,
}

#[event]
pub struct BatchTransferCompleted {
    pub total_amount: u64,
    pub transfer_count: u8,
    pub remaining_prize: u64,
    pub timestamp: i64,
}

#[event]
pub struct SolWithdrawn {
    pub amount: u64,
    pub authority: Pubkey,
    pub timestamp: i64,
}

#[error_code]
pub enum LotteryError {
    #[msg("Lottery is currently locked")]
    Locked,
    #[msg("Insufficient token amount for ticket purchase")]
    InsufficientAmount,
    #[msg("Invalid ticket numbers: must be between 1 and 33")]
    InvalidTicketNumbers,
    #[msg("Invalid draw time - must be at the start of each hour")]
    InvalidDrawTime,
    #[msg("Insufficient balance for withdrawal")]
    InsufficientBalance,
    #[msg("Invalid token mint address")]
    InvalidTokenMint,
    #[msg("Must wait minimum interval between draws")]
    DrawTooEarly,
    #[msg("Already drawn this hour")]
    AlreadyDrawn,
    #[msg("Prize update window is closed")]
    PrizeUpdateWindowClosed,
    #[msg("Insufficient prize amount in pool")]
    InsufficientPrizeAmount,
    #[msg("Arithmetic operation failed")]
    ArithmeticError,
    #[msg("Prize amount must be greater than 0")]
    InvalidPrizeAmount,
    #[msg("Cannot buy tickets during draw window")]
    DrawWindowActive,
}

impl LotteryState {
    pub fn is_in_draw_window(&self, current_time: i64) -> bool {
        (current_time / SECONDS_PER_MINUTE) % MINUTES_PER_HOUR <= DRAW_WINDOW_MINUTES
    }

    pub fn can_draw(&self, current_time: i64) -> bool {
        let time_since_last_draw = current_time - self.last_draw_time;
        time_since_last_draw >= MIN_DRAW_INTERVAL && !self.is_locked
    }
}

fn validate_ticket_numbers(numbers: &[u8; NUMBERS_COUNT]) -> bool {
    numbers
        .iter()
        .all(|&num| num >= MIN_NUMBER && num <= MAX_NUMBER)
}

fn generate_vrf_random_number(
    timestamp: i64,
    recent_blockhashes: &[u8],
    lottery_account: &AccountInfo,
    uuid: &str,
) -> Result<[u8; 32]> {
    let mut data = Vec::with_capacity(512);
    data.extend_from_slice(uuid.as_bytes());
    data.extend_from_slice(&timestamp.to_le_bytes());
    data.extend_from_slice(recent_blockhashes);
    data.extend_from_slice(&lottery_account.data.borrow());
    data.extend_from_slice(&lottery_account.lamports().to_le_bytes());

    if let Ok(clock) = Clock::get() {
        data.extend_from_slice(&clock.slot.to_le_bytes());
    }

    let mut final_hash = hash(&data).to_bytes();
    for _ in 0..3 {
        final_hash = hash(&final_hash).to_bytes();
    }

    Ok(final_hash)
}

fn convert_random_to_3d_numbers(random_value: &[u8; 32]) -> [u8; NUMBERS_COUNT] {
    let mut numbers = [0u8; NUMBERS_COUNT];
    for i in 0..NUMBERS_COUNT {
        numbers[i] = (random_value[i] % MAX_NUMBER) + 1;
    }
    numbers
}
