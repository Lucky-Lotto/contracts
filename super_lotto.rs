use anchor_lang::prelude::*;
use anchor_lang::solana_program::hash::hash;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};
use solana_program::clock::Clock;

declare_id!("4hHb7msxJiSY52LToCS1vvQd4friFRQkKyuK74HhNPgv");

pub const LOCK_DURATION: i64 = 600; // 10 minutes lock period
pub const DRAW_START_TIME: i64 = 0; // UTC 00:00:00
pub const DRAW_END_TIME: i64 = 600; // UTC 00:10:00

#[program]
pub mod lottery_contract {
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
        lottery.last_draw_numbers = [0; 7];
        lottery.last_prize_amount = 0;
        Ok(())
    }

    pub fn buy_ticket(ctx: Context<BuyTicket>, numbers: [u8; 7], amount: u64) -> Result<()> {
        let clock = Clock::get()?;
        let lottery = &mut ctx.accounts.lottery;

        require!(
            ctx.accounts.lottery_token_account.mint == lottery.token_mint
                && ctx.accounts.buyer_token_account.mint == lottery.token_mint,
            CustomError::InvalidTokenMint
        );

        if lottery.is_locked {
            let time_since_last_draw = clock.unix_timestamp - lottery.last_draw_time;
            require!(
                time_since_last_draw > LOCK_DURATION,
                CustomError::LotteryLocked
            );
            lottery.is_locked = false;
        }

        require!(
            amount >= lottery.min_purchase_amount as u64,
            CustomError::InsufficientAmount
        );
        require!(
            validate_ticket_numbers(&numbers),
            CustomError::InvalidTicketNumbers
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
        });

        Ok(())
    }

    pub fn draw(ctx: Context<Draw>, uuid: String) -> Result<()> {
        let clock = Clock::get()?;
        let current_timestamp = clock.unix_timestamp;
        let day_start = (current_timestamp / 86400) * 86400;
        let seconds_from_day_start = current_timestamp - day_start;

        require!(
            seconds_from_day_start >= DRAW_START_TIME && seconds_from_day_start <= DRAW_END_TIME,
            CustomError::InvalidDrawTime
        );

        let lottery = &mut ctx.accounts.lottery;

        require!(!lottery.is_locked, CustomError::AlreadyDrawn);

        let recent_blockhashes = ctx.accounts.recent_blockhashes.try_borrow_data()?;
        let lottery_info = lottery.to_account_info();
        let random_value = generate_vrf_random_number(
            current_timestamp,
            &recent_blockhashes,
            &lottery_info,
            &uuid,
        )?;

        let draw_numbers = convert_random_to_numbers(&random_value);

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
            CustomError::PrizeUpdateWindowClosed
        );
        require!(prize_amount > 0, CustomError::InvalidPrizeAmount);

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
            .ok_or(CustomError::InsufficientBalance)?;

        require!(
            amount <= available_balance,
            CustomError::InsufficientBalance
        );

        **ctx
            .accounts
            .lottery
            .to_account_info()
            .try_borrow_mut_lamports()? -= amount;
        **ctx.accounts.authority.try_borrow_mut_lamports()? += amount;

        Ok(())
    }

    pub fn transfer_token<'info>(
        ctx: Context<'_, '_, '_, 'info, TransferToken<'info>>,
        transfers: Vec<TransferInfo>,
        total_amount: u64
    ) -> Result<()> {
        require!(total_amount > 0, CustomError::InsufficientPrizeAmount);
        require!(
            ctx.accounts.lottery.last_prize_amount >= total_amount,
            CustomError::InsufficientPrizeAmount
        );

        let auth_key = ctx.accounts.authority.key();
        for (i, transfer) in transfers.iter().enumerate() {
            let recipient_account = ctx
                .remaining_accounts
                .get(i)
                .ok_or(CustomError::InvalidTokenMint)?;

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
            .ok_or(CustomError::ArithmeticError)?;

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
pub struct LotteryState {
    pub authority: Pubkey,
    pub token_account: Pubkey,
    pub token_mint: Pubkey,
    pub last_draw_time: i64,
    pub is_locked: bool,
    pub min_purchase_amount: u32,
    pub last_draw_numbers: [u8; 7],
    pub last_prize_amount: u64,
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
pub struct Initialize<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + 32 + 32 + 32 + 8 + 1 + 4 + 7 + 8,
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
        token::authority = lottery,
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

#[derive(AnchorSerialize, AnchorDeserialize)]
pub struct TransferInfo {
    pub recipient: Pubkey,
    pub amount: u64,
}

#[event]
pub struct TicketPurchased {
    pub buyer: Pubkey,
    pub numbers: [u8; 7],
    pub amount: u64,
}

#[event]
pub struct PrizeAmountUpdated {
    pub amount: u64,
    pub draw_time: i64,
}

#[event]
pub struct DrawResult {
    pub numbers: [u8; 7],
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

#[error_code]
pub enum CustomError {
    #[msg("Lottery is currently locked")]
    LotteryLocked,
    #[msg("Insufficient token amount for ticket purchase")]
    InsufficientAmount,
    #[msg("Invalid ticket numbers: red balls (1-33), blue ball (1-16)")]
    InvalidTicketNumbers,
    #[msg("Invalid draw time - must be between UTC 00:00:00 and 00:10:00")]
    InvalidDrawTime,
    #[msg("Transfer window closed")]
    TransferWindowClosed,
    #[msg("Insufficient balance for withdrawal")]
    InsufficientBalance,
    #[msg("Invalid token mint address")]
    InvalidTokenMint,
    #[msg("Draw cannot be performed yet")]
    DrawTooEarly,
    #[msg("Already drawn today")]
    AlreadyDrawn,
    #[msg("Prize update window is closed (10 minutes after draw)")]
    PrizeUpdateWindowClosed,
    #[msg("Insufficient prize amount in pool")]
    InsufficientPrizeAmount,
    #[msg("Arithmetic overflow error")]
    ArithmeticError,
    #[msg("Prize amount must be greater than 0")]
    InvalidPrizeAmount,
    #[msg("Invalid transfer count, must be between 1 and 10")]
    InvalidTransferCount,
}

fn validate_ticket_numbers(numbers: &[u8; 7]) -> bool {
    let mut used_reds = std::collections::HashSet::new();

    for &num in numbers.iter().take(6) {
        if num < 1 || num > 33 || !used_reds.insert(num) {
            return false;
        }
    }
    numbers[6] >= 1 && numbers[6] <= 16
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

fn convert_random_to_numbers(random_value: &[u8; 32]) -> [u8; 7] {
    let mut numbers = [0u8; 7];
    let mut used_reds = std::collections::HashSet::new();

    for i in 0..6 {
        let mut val = (random_value[i] as u16 % 33 + 1) as u8;
        while used_reds.contains(&val) {
            val = (val % 33) + 1;
        }
        used_reds.insert(val);
        numbers[i] = val;
    }

    numbers[6] = (random_value[6] % 16 + 1) as u8;

    numbers
}
